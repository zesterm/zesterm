# zesterm roadmap

Current state and open work, nothing else. What the system *is* — the goal, the
layers, the crate map — lives in [ARCHITECTURE.md](ARCHITECTURE.md)'s "The
system" overview; the seams that must not move are in
[CONTRACTS.md](CONTRACTS.md). Update this file in the same commit as the work
it describes. Completed work is deleted here rather than archived — git
history, closed PRs and closed issues keep the record.

## Status

All seven gates green (see `AGENTS.md` § The gates). First paint 35ms **on
Windows**; the Mac paints against a different compositor and its number (48ms)
is reported rather than gated.

| Crate | State |
|---|---|
| `zest-pty` | ✅ ConPTY *and* unix (`openpt`), resize, shutdown, explicit `hangup`, `.vtrec` recorder |
| `zest-core` | ✅ grid, scrollback, VT, modes, OSC, palette, `ChangeSource`, `RemoteWriter`, command blocks from OSC 133/7/633 |
| `zest-font` | ✅ metrics, shaping, fallback, colour glyphs, Nerd Font PUA — the grid shapes runs when `typography.features`/`ligatures` ask for it, per-character otherwise |
| `zest-theme` | ✅ tokens, OKLCH derivation, 5 built-ins, 4 importers |
| `zest-render-wgpu` | ✅ pipelines, atlas, offscreen resolve, selection |
| `zest-config` | ✅ cascade, provenance, profiles, migrations, hot reload, JSON Schema — **every declared setting is consumed** (a test keeps `NOT_YET_WIRED` empty) |
| `zest-input` | ✅ keys + SGR mouse + selection + IME + Kitty CSI u (flags 1, 2, 8), Rust and TypeScript — ⬜ Kitty flags 4/16, keypad |
| `zest-app` | ✅ window, tabs (top strip / left sidebar) behind `SessionSource`, **attached to its own daemon**, fleet picker (⌘K), **split panes — any number, on any host** (⌘D splits on the window's host, ⌘H splits through the picker onto a machine or an existing session, ⌘U/⌘J move the keyboard; #436), restore-on-launch — runs on Windows *and* macOS (Metal, transparent titlebar), springs + smooth scroll + reduce_motion, cursor shapes (config *and* DECSCUSR) with a spring trail, **tabs that say what is happening in them** — close and detach in both positions, a busy ring from OSC 133 *or* OSC 9;4, and an attention dot from BEL / OSC 9 / OSC 777 that names no program, imported colour schemes as first-class themes (the gallery's import card pastes any of the 4 formats into the user theme dir) — ⬜ Snap Layouts, polish |
| `zest-proto` | ✅ protocol 3, encoder, `Applier` into a real `Terminal`, `GridView` for TS clients, framing, sealing, cell-for-cell conformance, chaos-resync, command blocks |
| `zest-mesh` | ✅ Ed25519 identity, keystore, mDNS discovery, layered fleet, pairing + trust store, sealed channel |
| `zest-fleet` | ✅ what a machine in the fleet is, how the two sources are merged into one row, and the one rule that picks how to reach it — pure, so every client shares the decision rather than a copy of it |
| `zest-cloud` | ✅ `TlsDuplex`, one connection as two independently owned halves, a one-request HTTP POST over it, `Endpoint` — consumed by `--enroll` and by `--relay`'s per-pipe dial-back |
| `zest-daemon` | ✅ session ownership *and* lifecycle, protocol loop, loopback / LAN / WebSocket / relay transports, real `Seq`/`Ack`, scrollback, socket locking, authentication, pairing, publishes its own profiles, reports what a child exited with, the account client every device shares — `fetch_hosts` and the one relay ladder, moved down out of the app so a second client can reach them |
| `zest-mcp` | ✅ reads, drives and runs terminals over MCP on stdio; `run` correlates a command in the user's own shell and `run_isolated` carries the unforgeable exit code; `screen` and `blocks` wait instead of the caller sleeping; `input` takes named keys and a paste, each its own keystroke; `sessions` asks the host rather than serving a cache, so a title, cwd and `alt_screen` describe the session now; reaches **every machine in the fleet** — mDNS plus the account directory, one connection per host dialled on first use, and a machine nothing can reach is listed with the reason rather than hidden |

### What works end to end today

A daily-drivable terminal on Windows and macOS — GPU-rendered, themed, hot
reload, selection, scrollback, Nerd Font prompts — where **the window is a
client of its own daemon**: close it and the shell keeps running, reopen and it
adopts the session it lost. A daemon serves other machines over the LAN
(`--listen-lan`, mDNS discovery, pairing with a matching code) and over the
relay when away (`--relay`, dial-back, sealed end to end), and a client
`Terminal` rebuilt from deltas is cell-for-cell identical to the host's at
every frame of the recorded corpus. Shells emitting OSC 133 (zsh, pwsh and
bash — WSL included — get it injected, nothing installed) produce **command
blocks** — what ran, where,
what it printed, how it ended — that cross the wire, drive copy-output/re-run,
and give an agent `blocks` instead of a build log. The fleet is visible and
launchable from one window: every reachable machine's sessions and published
launch profiles in the ⌘K picker, the fleet cards and the `+` menu. **The
browser is the same window**: signed in, it holds a connection per enrolled
machine and gets the tab strip, the sidebar, the palette and the same
host-grouped `+` menu — every machine's profiles, launchable, on a phone. The
hosted web client attaches to a local daemon over WebSocket, and `zest-mcp` gives any
agent harness the same terminals as tools, including `run_isolated`'s exit code
that nothing inside the terminal can forge.

## Open work

Grouped by area. Each item keeps only the constraint that makes it non-obvious;
the history behind them is in closed issues and PRs.

### Windows chrome & polish

- [ ] Snap Layouts: `HTMAXBUTTON` over the maximize rect enables the Win11
      hover flyout, and it needs a real window-proc subclass — `WM_NCHITTEST`
      is *sent*, not posted, so winit's `with_msg_hook` cannot see it. It also
      suppresses ordinary mouse messages over that rect, so hover must come
      from `WM_NCMOUSEMOVE`.
- [ ] Polish: OSC 0/2 title, font zoom, DPI changes. (DECSCUSR cursor styles
      and `cursor.shape`/`cursor.trail` are done; `smear` is #329.)
- [ ] Perf validation: vtebench, >500 MB/s, <2ms CPU frame, <10ms keypress→pixel.
- [ ] The tab-signal tail (#379–#385 left these deliberately): a right-click tab
      menu, which is where Detach belongs once there is one — today it is ⌘B, the
      palette row, and the busy confirm's button; **kitty's OSC 99**, whose
      `d=`/`i=` chunking is a parser of its own rather than another arm; and the
      notification *text* `OSC 9`/`OSC 777` supply, which is off the wire until
      something renders it, because a field nothing reads is indistinguishable
      from one nothing can fill.
- [ ] Panes after #436: they tile as equal columns only — no drag-to-resize,
      no rows, no tree — and a pane does not persist across a restart (the
      tab does, as its primary). Per-pane scroll wheel routing is still the
      focused pane's (`hit::wheel_target` swallows the rest), and a pane on a
      relayed host has no dial hint for restore, like a relayed tab.
- [ ] Background pictures after #450: the pipeline, the three `window.background_*`
      keys and their rows in Settings and the profiles editor are in, but the
      row is a path *text field* — #144's `<image-slot>` drop target and the OS
      drag-and-drop behind it are still open, and there is no file picker
      anywhere in `crates/` to build it on. The browser ignores the keys: it
      cannot read a native host's path, and `painter.ts`'s per-dirty-row
      `fillRect` of the default background would erase an image layer strip by
      strip.
- [ ] Render `SessionInfo.busy` in the ⌘K picker and the fleet cards. The
      field and the push behind it landed with #416 (`Registry`'s coalesced
      pulse also ends the stale-`title`/`cwd` watcher problem); what remains
      is the client-side dot/spinner on rows for sessions this window is not
      attached to.

### Input

- [ ] Kitty flags 4 (alternate keys) and 16 (associated text). 4 needs the
      base-layout key, which winit exposes through a trait that does not cover
      Wayland — a platform-capability question, not a table to fill in. 16 is
      what would let an IME commit reach a program running under flag 8.
- [ ] Keypad keys as separate keys under flag 1 (`CSI 57399…57427 u`). Left out
      rather than guessed: the numbers want checking against
      `kitty +kitten show_key -m kitty`, and wrong key numbers are worse than
      absent ones.
- [ ] `Ctrl+Tab` is swallowed by the binding table before the encoder sees it
      (`keymap.rs`, `When::Always`), so it cannot reach a program as `CSI 9;5u`.
      The fix is a third `When` variant, not an if-block.
- [ ] `CSI > c` (DA2) and `CSI = c` (DA3) are answered with DA1, from the same
      wildcard-intermediate mistake as the `u` arm ten lines away. Harmless
      today, but on the kitty probe path — fix deliberately, not as a drive-by.

### Unix hosts

- [ ] The remaining macOS polish tail.
- [ ] Linux: Vulkan surface, fontconfig fallback verification.
- [ ] Linux: negotiate `zxdg_toplevel_decoration_v1` or KDE gives you *two*
      titlebars.
- [ ] Linux: transparency via an ARGB visual. **Blur has no portable path** —
      X11/KWin has `_KDE_NET_WM_BLUR_BEHIND_REGION`, picom needs user rules,
      Wayland has no protocol. Degrade honestly rather than pretending in the
      settings UI.
- [ ] Linux packaging.

### Shell integration & blocks

- [ ] **fish, and the shells WSL bash left behind** (#405 landed bash, native
      and through `wsl.exe -d <distro> -- bash`). fish is deliberately
      unwritten: it cannot be *seen working* on the machines this is built on,
      and writing it blind is how features ship compiled and unseen. Around
      WSL, three declined-with-a-log cases could become features: an inner
      *zsh* (its `ZDOTDIR` would have to ride `WSLENV` like
      `ZESTERM_BASH_INIT` does), a bare `wsl.exe` (needs the distro's default
      shell discovered rather than guessed), and Git Bash's `bash.exe` (MSYS
      rewrites unix-looking arguments; untested against `--init-file`).
      (`cmd.exe` is a permanent no: it has no prompt-function mechanism.)
- [ ] **A settings key for shell integration.** Today it is a daemon flag,
      which is not where anyone will look. The shell runs on the *host*, so the
      host decides — closing this means `zest-daemon` reading settings or a new
      field on the frozen `CreateSession`, and neither is worth doing before
      someone wants the switch.
- [ ] **The `/etc/zshenv` hole.** A system `zshenv` that re-sets `ZDOTDIR` runs
      *after* our environment and silently undoes the injection — Ghostty
      documents having no fix, kitty tracks it as their #6330. It wants
      detecting and reporting rather than looking like a shell that emits no
      markers.

### Prompt context widgets

The data spine landed with #416: the daemon computes a `SessionContext` per
session (git branch/detached from a HEAD read, kube current-context, version
pins — file reads only, cached per cwd, invalidated by `notify` watchers) and
publishes it on `SessionInfo` beside `busy`, so every client and `zest-mcp
sessions` see identical facts, each labeled `daemon_probe` or `shell_report`.
The shell-reported half landed with #418 (PR #419): the injected hooks emit `Venv`,
`Conda`, `AwsProfile` and `NvmBin` over OSC 633 `P;Key=Value` — parameter
expansion only, a changed-value cache so an unchanged prompt emits nothing,
an empty value taking the chip down — and the listing folds them in as
`shell_report`, the active node replacing the `.nvmrc` pin. (These ride the
session listing, not the delta stream, so the conformance corpus is not
involved — it becomes so when a block *snapshot* carries them, below.)
What remains, in landable slices:

The chips themselves landed with #420: `chrome/prompt_chips.rs` draws them on
the blank row above the live prompt (right-margin fallback that hides before
typing reaches it), the web BlocksPane renders the same set on its prompt
item, `prompt.widgets` (tag-list) picks which show natively, a click copies
the value, and the exit chip selects its failed block. Both design-doc
stances (§no-status-bar, never-overlay-live-prompt) are annotated rather than
broken.

The prompt became the chips with #426 — and the default with #435 (the
framework guard is what makes that safe; a curated PS1 is never touched):
`prompt.compact_ps1`, on by default and opt-out (read by
the daemon at spawn — the shell runs on the host, so the host decides — and
declined when p10k/starship/oh-my-posh own PS1) collapses PS1 to a blank
line + `❯`, the blank row being exactly the chip layout's preferred home; the
cwd chip opens a recent-directories menu on the block-menu chassis (`cd`
typed through the ordinary input path, single-quote wrapping, gated on the
live prompt at build *and* click); and the exit chip reveals its failed block
via `Grid::scroll_to_line`, which lives in the grid because wrapping makes
row count and line-id distance disagree.

- [ ] **Web-side `prompt.widgets` filtering**, once a browser has somewhere
      to read another machine's display preference from — or a decision that
      chips shown in the browser are simply all of them.
Per-block context landed with #429: the daemon stamps each starting command
with `BlockContext { branch, venv, kube }` (core carries it like `cwd`;
`BlockPayload.context` rides additively), so "that failing build ran on
branch X" survives into scrollback — the branch shows in both clients'
block headers, and `blocks` over MCP returns the snapshot per block.
The dirty flag and the link chip landed with #432: `git status --porcelain
-uno` on a bounded background thread (TTL re-ask on listing pulls, HEAD
watcher invalidation, failures stretching the cadence), feeding
`GitContext.dirty`/`changed` so the git chip reads `main`, then `main*`,
then `main* ±3` in the order the answers arrive; and the `link` widget
(off the default list) shows `lan 0.4 ms` / `relay 62 ms` from the app's
own fleet model, loopback deliberately silent.

The cwd chip grew its directory browser with #439: `ListDir`/`DirListing`
on the wire (the `Enroll` additive rule), so the same picker serves a
remote tab over the LAN or relay — search-as-you-type, `..` up, ⏎ or click
switches (the `cd_bytes` gates), ⇥ browses without committing, and the
in-process window answers itself. The web client has the wire pair; its
picker UI is still open below.

- [ ] **Switcher chips** (branch, kube): the `ListDir` precedent shows the
      shape — a request/reply pair each — but a branch list also wants the
      write gated the way `cd` is, so design the typing story first.
- [ ] **The browser in the browser**: the hosted client's cwd-chip picker,
      on the `DirListing` pair the wire now carries.
- [ ] **Real runtime versions** (`node --version` and kin), cached per
      binary path: the pins say what was asked for; only a subprocess knows
      what answers.

### Editor & code review

Warp has two surfaces zesterm lacks — a built-in editor and a panel showing
what changed — and they share one problem: a session runs on any host, so
"open this file" has to be a question its own daemon answers about *its* disk.
A local-only editor is the half-feature this roadmap declines. Epic: #445.

- [x] **A file can be read and written on any host** (#446). `ReadFile` /
      `FileContents` and `WriteFile` / `FileWritten`, on `ListDir`'s pattern:
      correlation by echoing the path, a string `error` so a refusal is not
      confusable with an old daemon's, and the whole answer computed in the
      dispatch arm because a read is bounded by its cap. A save carries the
      hash it was based on and is *refused* rather than obeyed when the disk
      moved underneath — and a truncated read carries no hash at all, so a
      buffer holding the first few megabytes of a larger file cannot save over
      the rest of it.
- [x] **`GitDiff`** (#453), for the review panel: the repo's uncommitted
      changes as raw unified text — staged *and* unstaged against HEAD, so the
      panel has one truth rather than two lists a person adds up — plus the
      untracked names `git diff` structurally cannot show. Truncation drops
      whole files, never half a hunk. A subprocess with a deadline, so unlike
      the file reads it answers off a worker through a deferred-reply mailbox:
      the serve loop holds the connection lock across `on_bytes`, and a slow
      repository would otherwise stall that session's own input and output.
      `gitcmd::run_git` is the bounded-subprocess skeleton, extracted from the
      context engine's dirty probe so there is one copy of "run git without
      letting it hang the daemon".
- [ ] **A pane can be something other than a session.** `PaneContent` becomes a
      sum type so a tab can hold an editor beside its shell; no runtime change.
- [ ] **A file opens in a pane**: the buffer, gutter, ⌘S, and the conflict
      answer that offers reload-or-overwrite rather than picking one. Design
      §13.
- [ ] **Syntax highlighting**, coloured from the theme's own tokens and ANSI
      row in OKLCH rather than from a bundled highlighter theme that would
      clash with every zesterm palette.
- [ ] **Three ways to open a file**: a modifier-click on a path in output
      (reconciled with the copy-block-output gesture that chord already has), a
      palette command, and a ⋯ row when the block's command named a file.
- [ ] **The review panel**: uncommitted changes grouped by file, expandable,
      opening into the editor at the line. Staging is out. Design §14.

### Protocol & daemon

- [ ] **Assert client scrollback equals the host's.** `SbPush` is emitted only
      when the encoder calls a viewport move a scroll, and a jump larger than
      the viewport deliberately is not one — so the host can push history the
      client is never told about. Nothing checks this, which is why the
      fixtures carry no scrollback expectation: it would pin a divergence
      rather than catch it.
- [x] **The corpus has three holes** (#17): closed by three ConPTY recordings
      — `astral`, `combining-marks`, `scroll-flood` — replacing the synthetic
      fixtures, with a census test in `conformance.rs` so none of the three
      can silently reopen.
- [ ] SQLite scrollback. Scrollback is in memory and bounded; a session that
      outlives its window does not yet outlive the daemon.
- [ ] **Local echo prediction** (mosh's other trick; #442, ADR-016). The
      engine landed first: `zest-proto::predict` and its TypeScript port
      reconcile guesses from a delta's own rows and cursor, held to one rule
      set by `fixtures/predict.json`. The native window draws them
      (`Viewport.predicted` beside `preedit`; `SessionSource::predict` takes
      the key *before* `key::encode`; `cursor.predict_echo` is
      `auto|always|off`; `--simulated-latency <ms>` holds every host update
      on the reader so the edit-run loop needs no relay). The browser draws
      them too: `SessionClient` owns the port's `Predictor`, fed the key
      beside the bytes, and the guess is a `predicted` span on the DOM
      prompt row — the canvas painter is untouched, because the alternate
      screen is never guessed into. What remains: the measured echo latency
      feeding the `link` chip, which today shows a TCP-connect probe; a
      browser-side policy once a browser has somewhere to read a preference
      from (it is `auto` with the remote hint on, which a loopback daemon
      turns off by measurement); guesses through a wrap; and — only once the
      heuristic is seen failing on a real link — an additive `echo` sequence
      on `Input`/`Update`, which is what would let "not echoed yet" be told
      from "never echoed". Seen working in tests on both ports; the loopback
      web rig could not show it live because of #447.

### Web client & devices

- [x] **The browser is host-plural, and the fleet is in the chrome.** The
      hosted path was three screens inside one component with no tabs, no
      launcher and no palette; it mounts the same `Shell` the loopback path
      does, over a `HostSource` that answers for every enrolled machine.
      Profiles cross the wire (`Hello.watch_hosts` → `Sessions.offer`,
      ADR-014), so the `+` menu groups launch targets by the machine that will
      run them and `⌘⇧,` opens them read-only — every profile it can see lives
      in the config of the machine that publishes it, and editing happens
      there. #332, #338, #342, #351, #352.
- [x] **The device registry UI** — the account's list of machines and browsers
      (`Fleet.tsx` + `registry.ts` over `/api/enroll/*`, `/api/hosts`,
      `/api/devices` and the revoke routes), including recovery: revoked rows
      stay visible in a Revoked section and `POST /api/{hosts,devices}/:id/restore`
      puts one back — the machine's stored token simply resolves again. #365.
- [x] **The browser client under a finger, part 1 — the keyboard opens.** On
      an iPad the hidden textarea took focus off a deferred call, which iOS
      answers with no keyboard; a synchronous focus in the tap's own task
      fixes it, the textarea is 16px so the page stops zooming, and
      `visual-viewport.ts` sizes the shell to the *visual* viewport so the
      prompt stays above the keys. #421.
- [x] **Part 2 — the key bar.** The phone design's cap row (`esc` `tab`
      `ctrl` `alt` arrows `⏎` `^C` `/ - | ~` `⌨`) under every
      terminal, on by default where the keyboard is on the glass
      (`maxTouchPoints`, never the platform string) and toggled from the
      palette. Caps feed `encodeKey` a synthesised key, so DECCKM and
      kitty come for free; sticky Ctrl/Alt latch once or lock and ride the
      next soft-keyboard key too. `keybar-model.ts`, #421.
- [x] **Part 3 — tap to answer.** Numbered option rows of the running
      block — an agent CLI's question, a menu — are tappable and type their
      digit (only the digit: the program decides what it means). Touch
      only, or where the key bar is on; a mouse click on output still means
      "focus". `optionOf` in `blocks-pane-model.ts`, #421.
- [x] **Part 4 — it opens on the device.** Part 1's synchronous `focus()`
      was a no-op on a real iPad: the textarea is focused at mount, and iOS
      opens the keyboard only for a focus *change* in the gesture. A touch on
      a focused terminal now blurs and refocuses, ⌨ reads keyboard-up from
      the visual viewport rather than `activeElement` (iOS's dismiss key does
      not blur), and the bar is 44px over the keyboard instead of ~90
      (`soft-keyboard.ts`, #428).
- [ ] Browser device enrollment: non-extractable Ed25519 key, approved via the
      desktop modal with a matching code.
- [ ] Bun single-file sidecar hosting `@sigx/actors`, spawned as a child of the
      daemon, length-prefixed msgpack over stdio. Never in the PTY hot path.
- [ ] **The web client learns a second data plane.** `DataPlane` grows a
      discriminant and a relay `Dial` mints its ticket before opening the
      socket (the seam stays synchronous — a failed mint is a dropped dial).
      The seam (`directory-source.ts`) landed with only the loopback
      implementation behind it; the store half is blocked on the ticket
      endpoint and on any notion of which hosts are online, and wiring the
      cloud branch before those exist would replace an honest card with a list
      that reconnects for ever.

### Security & remote access

- [ ] Remote access **off by default**, persistent indicator, audit log.
- [ ] Relay hardening, both gaps stated in the code: the daemon reads and logs
      the relay's `relay_key` but pins it to nothing (survivable because the
      sealed channel inside the pipe never trusts the relay), and a relayed
      pipe takes a mid-handshake slot but feeds no rate limiter — the only key
      available is the edge's address, and the fix needs a key the peer owns,
      which is its attach ticket, which the daemon never sees.

### Agents

- [x] **`run`, into the user's interactive shell** — with their venv, ssh-agent
      and kubectl context. OSC 133 `D` parsed host-side carries the shell's own
      exit code; a timeout does not kill — the block stays `running` and
      partial output comes back, so a command sitting at `Password:` can be
      answered, the case a sentinel-injecting harness cannot tell from success,
      and `blocks(wait:)` follows it from there. The correlation is
      `block_anchor`/`finished_since`, not a second copy: OSC 133 `C` mutates
      the *existing* trailing prompt block, so the anchor is the tail block's
      identity before the write. Writing adds the states a wait does not need —
      a command the shell never started, a block a screen clear destroyed — and
      the refusals it does: an alt screen, a shell emitting no markers, a
      command already running, and the gap between `D` and the next prompt,
      which two `run`s back to back land in almost every time. `warnings` say
      when the block records a different command than the one sent, or none at
      all. → ADR-015.
- [x] **Named keys, and every part its own keystroke.** An agent has no
      keyboard, so `input` takes `keys: ["down","down","enter"]` and encodes
      them host-side — an arrow is `ESC [ A` or `ESC O A` depending on DECCKM,
      which lives on the host, so a hand-written sequence reached the
      application roughly 2 attempts in 10 and arrived as literal text the
      rest. Unknown names refuse with the vocabulary; a key that silently does
      nothing is indistinguishable from one the app ignored. `text`, `paste`,
      each key and `submit`'s Enter are separate writes — sharing one made a
      TUI read the whole thing as a paste and drop the CR into its composer.
      Splitting is *necessary and not sufficient* (a tty hands the next read
      everything queued), so `paste` carries the boundary in the byte stream
      instead; it is a separate argument and never inferred from `text`,
      because DEC 2004 is set for a program's whole run and auto-wrapping
      `:wq` for `nvim` would insert it rather than run it. The table is the
      third copy of one rule, held byte-for-byte against `zest-input` by
      `tests/keys.rs` rather than by review. → #344, #345.
- [x] **Dim text is not typed text.** `screen` carries `styled` —
      `{row, col, len, attrs}` — because flattened to characters, text an
      application is *offering* is identical to text the user committed: a
      CLI's greyed suggestion read as a pending instruction, one Enter from
      acting on words nobody wrote. It also recovers a picker's selection when
      that is drawn by inverting a row rather than printing a marker, which is
      the difference between navigating a dialog and aiming it. Positions and
      flag names, never text — attributed runs would restate the screen a
      second time at 3-5x the tokens, where spans measured 2-23 bytes across
      the corpus — so the value carries nothing a terminal authored and needs
      no fence. Always present rather than opt-in, since a signal behind a flag
      is absent exactly when it was wanted. No colour, and the three layout
      bits masked out. → #348.
- [x] **The fleet, as an agent's tools.** `hosts` lists every machine mDNS or
      the account knows, `sessions`/`create_session`/`run_isolated` take one,
      and every other tool already carried it inside the session id. One
      connection per machine, dialled on first use.
      **Not** gated on a host advertising the observer attach, which this line
      used to promise: `Attach.observe` degrades safely by construction — the
      agent votes the size the listing reports, so a daemon predating the field
      counts a no-op vote — and `HostOffer.features` would have bought a way to
      *know* rather than a way to be *safe*, at the cost of a field
      contradicting the struct it lives in ("Facts, deliberately, and not a
      capability matrix"). → #274.
- [x] **A durable `agent-key`, and a dial that does not cancel its own prompt.**
      A third principal beside `host-key` and `client-key`, so a host revokes
      the agent without revoking a laptop; read from the keychain on the first
      *remote* dial and never at startup. The first dial to an untrusted machine
      parks on a thread while the call answers with the six-digit code —
      `PendingHandle::Drop` cancels the request, so refusing by hanging up
      deletes the prompt it is reporting. → ADR-015.
- [x] **Tokens per build, measured.** `cargo run -p zest-mcp --example
      token_probe -- --cmd "<command>"` runs a command on a real pty and
      reports four numbers: the raw stream, the framed deltas, `screen`'s text
      and `output` per block. It spawns rather than replaying, because the
      corpus has no build in it — its largest recording is 10 KB of vim.
      The last two come from a real `Replica` fed the encoder's own output, so
      they are what a tool returns rather than a second reading of the grid.
      **The two numbers behave differently, which is the finding.** For
      `seq 1 200000` — 1.49 MB of pty, ~596k tokens — the model-facing answer
      is 202 bytes, ~51 tokens, and it does not move: `screen` is the final
      grid, so it is bounded by the grid rather than by how much was printed.
      The transport figure moves by two orders of magnitude with polling
      cadence alone, because `zest-proto` coalesces on *state*: one poll is
      3,254 bytes (reproducing ADR-004's ~3 KB and confirming that figure is
      the single-delta regime), a 16 ms client is 507 KB, and asking after
      every read is 11.4 MB — larger than the byte stream it replaces. So
      ADR-004's number is a floor for an idle observer, not a saving every
      client receives, and the agent-facing number is the stable one. A
      `cargo build --workspace` is 40 KB of pty (~16k tokens) against 1,667
      bytes of `screen` (~417) — the tail, which is what "did it build" wants.
- [ ] **Provenance.** An author on `Block`, so scrollback records who ran what.
      Needs the daemon to stop forgetting: `welcome()` reads the `ClientId` and
      then `Gate::Served` drops the transcript. Core cannot hold a `ClientId`
      (`zest-proto` depends on `zest-core`, not the reverse), so it holds 32
      opaque bytes and the wire converts, as `LineId` becomes `i64` today.
- [ ] **An agent may not approve devices.** `may_approve_devices()` is a
      property of the *transport* alone, so any loopback client can answer
      `PairingDecision` and enrol an arbitrary remote key, unattended. Worth
      closing while a general local gate is not: a prompt-injected
      *cooperating* agent has only the tools it was given.
- [ ] Per-block consent and redaction, in `zest-core`, masking the delta so
      every client sees one masked truth — ADR-015's amendment records why a
      prompt-boundary filter is rejected. Plus fleet-wide block search and the
      agent pane.

**Deliberately not built:** no chat sidebar; no agent loop of our own
(harnesses improve monthly and a terminal shipping an inferior one ages badly —
be the substrate); nothing that delivers output to the agent with **no call
outstanding**, whose absence is what keeps prompt injection needing the agent
to be steered rather than firing on its own; no scrollback in the cloud by
default.

The line is at the call, not at the waiting: `screen(after_seq:)` and
`blocks(wait:)` block until something happens, because a read the agent asked
for cannot manufacture a turn. ADR-015 carries the argument, amended once
already — it read "no streaming *or polling* tool", which forced
sleep-and-re-read and so pushed *more* attacker-controlled output through the
model per unit of progress watched.

### Phone

- [ ] Lynx app. **Blocks-first, not grid-first** — a phone is excellent at
      lists, and you drop into grid view only when `alt_screen` is true, which
      the host already reports. Sticky `Ctrl` toggle, local history from the
      block index, long-press to re-run. Designed →
      `docs/design/phone/README.md`; the one open piece is grid rendering on
      Lynx (no canvas package at 0.26), and blocks-first is what makes that
      deferrable.

## Dogfooding

zesterm must correctly host `@sigx/terminal` TUIs — alt-screen, truecolor, raw
mode, resize, cursor and erase. Use `examples/showcase` and
`examples/claude-shell` from `C:\Dev\sigx\terminal\main` as acceptance content.

Theme `ui.*` tokens are `@sigx/terminal-zero`'s contract verbatim, so one theme
file styles zesterm's chrome *and* any sigx TUI running inside it.

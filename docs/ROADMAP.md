# zesterm roadmap

The durable plan. Mirrored as tracking issue
[#1](https://github.com/zesterm/zesterm/issues/1), but **this file is the source
of truth** — update it in the same commit as the work it describes, then refresh
the issue.

## The goal

A terminal in the Warp performance class — GPU-rendered, low input latency,
deeply themable — on Windows, macOS and Linux. And then the part that makes it
worth building: **every machine reachable from every device.**

Not one machine exposed to the internet. A fleet. The Mac's shell in a window on
the Windows box. A Linux build watched from a phone on a train. Sessions that
outlive the window they were started in, picked up wherever you are.

That goal dictates everything. A terminal built as a monolithic GUI app cannot
grow a remote head without a rewrite, because its state lives inside the
renderer. So `zest-core` is headless and knows nothing about pixels; the daemon
owns sessions on every machine; and the native app is a client of its own daemon
exactly as the phone is a client over the network.

Start with [ARCHITECTURE.md](ARCHITECTURE.md) for the decisions that were
expensive to reach, and [CONTRACTS.md](CONTRACTS.md) for the seams that must not
move.

```
        ┌── phone (Lynx)          ┐
        ├── browser (SignalX)     │  clients
        ├── zesterm.exe (Windows) ┘
        │
        │   LAN direct (~0.3ms) where possible,
        │   Cloudflare Tunnel (~60ms) when away
        │
   ┌────┴──────────┬──────────────┬───────────────┐
   │ zest-daemon   │ zest-daemon  │ zest-daemon   │  hosts
   │ (Windows)     │ (Mac)        │ (Linux)       │
   │  PTYs, grid, scrollback, command blocks      │
   └──────────────────────────────────────────────┘

   Cloudflare holds a *directory* only: which hosts exist, are they up,
   how to reach them. No grid, no scrollback, never in the data path.
```

## Status

**600 tests, six gates green**, measured on macOS rather than remembered.
First paint 35ms **on Windows**; the Mac paints against a different compositor
and its number (48ms) is reported rather than gated.

| Crate | State |
|---|---|
| `zest-pty` | ✅ ConPTY *and* unix (`openpt`), resize, shutdown, explicit `hangup`, `.vtrec` recorder |
| `zest-core` | ✅ grid, scrollback, VT, modes, OSC, palette, `ChangeSource`, `RemoteWriter`, command blocks from OSC 133/7/633 |
| `zest-font` | ✅ metrics, shaping, fallback, colour glyphs, Nerd Font PUA |
| `zest-theme` | ✅ tokens, OKLCH derivation, 5 built-ins, 4 importers |
| `zest-render-wgpu` | ✅ pipelines, atlas, offscreen resolve, selection — ⬜ gamma validation |
| `zest-config` | ✅ cascade, provenance, profiles, migrations, hot reload, JSON Schema |
| `zest-input` | ✅ extracted; keys + SGR mouse + selection + IME — ⬜ Kitty protocol |
| `zest-app` | ✅ window, sessions behind `SessionSource`, **attached to its own daemon** — runs on Windows *and* macOS (Metal) — ⬜ macOS chrome |
| `zest-proto` | ✅ protocol 2, encoder, `Applier` into a real `Terminal`, `GridView` for TS clients, framing, cell-for-cell conformance, chaos-resync, command blocks |
| `zest-mesh` | ✅ Ed25519 identity, keystore, mDNS discovery, layered fleet, pairing + trust store — ⬜ Cloudflare transport (M4) |
| `zest-daemon` | ✅ session ownership *and* lifecycle, protocol loop, loopback *and* LAN transports, real `Seq`/`Ack`, scrollback, socket locking, authentication, pairing |

### What works end to end today

- A terminal you can use on Windows: themes, settings with hot reload,
  selection, scrollback, Nerd Font prompts, 35ms to first paint.
- The same terminal on macOS: Metal, a real `zsh`, truecolor, wide CJK, colour
  emoji, box drawing and Nerd Font icons, 48ms to first paint.
- **The window is a client of its own daemon.** Everything above renders
  identically with the shell in another process, reached over a unix socket as
  grid deltas. Close the window and the shell keeps running.
- `zest-daemon` serving a session over a named pipe or unix socket, with a
  client attaching and receiving live output as deltas
  (`cargo run -p zest-daemon --example attach`).
- A client `Terminal` reconstructed from those deltas that is **cell-for-cell
  identical to the host's** at every frame of six recorded sessions, and that
  converges again after a dropped frame at any of 10,000 points.
- Two machines minting verifiable identities and finding each other by mDNS
  (`cargo run -p zest-mesh --example mesh_probe`).
- **A daemon serving other machines**: `zest-daemon --listen-lan` binds TCP,
  advertises what it bound, and serves only devices that prove a key and are
  trusted. An unknown device makes the host print a matching code and wait for
  a person (`cargo run -p zest-daemon --example pair`).
- **Scrollback as a list of commands.** A shell emitting OSC 133 turns the grid
  into command blocks — what ran, where it ran, what it printed, how it ended —
  and they cross the wire to an attached client. Verified over a real socket
  with a real `zsh`: a success, a failure with `exit 1`, and a command still
  running with no end line, each with its cwd from OSC 7.
- **The shell says so itself, with nothing to install.** `zsh` gets zesterm's
  OSC 133 hook through a `ZDOTDIR` shim that sources the user's own dotfiles
  and writes none of them, so blocks appear against whatever prompt they
  already had. VS Code's OSC 633 is understood too, for anyone who has its
  integration.
- **Acting on a block.** `Cmd`/`Ctrl+Shift` + `O` copies what the last command
  printed — its output alone, not the prompt and not the command — and `R` runs
  it again. The same chord plus a click does it for any block in scrollback.

### Reflow

Resizing the width rewraps, rather than truncating and losing the text. A
*logical line* — rows joined by `wrapped`, which is what the program actually
printed — is rejoined and re-broken at the new width, so narrowing a window and
widening it again restores the screen exactly.

Two rules that are not obvious and are load-bearing:

- **The alternate screen is never reflowed.** A full-screen program repaints on
  `SIGWINCH` and its frame is a picture, not a paragraph.
- **Line ids are renumbered**, because rewrapping changes how many rows a
  logical line occupies and no one-to-one mapping exists. They stay monotonic
  top to bottom, which is what scroll detection and `lines_by_id` depend on.
  Anything anchored to an id must re-anchor, and `Grid::resize` returns a
  `Reindex` saying where each one went. The selection is *cleared* rather than
  mapped — it names a column as well as a line, and rewrapping moves both —
  while command blocks are re-anchored, because losing the block for a build
  because the window was widened while it ran is the case blocks exist for.

### The gap to M3

Three things at the level of features:

1. ~~**A LAN listener on the daemon**~~ **Done.** `--listen-lan` binds TCP,
   advertises the port it actually bound over mDNS, and serves only clients
   that prove a key and are trusted. Off by default.
2. ~~**Pairing**, so `listen_lan` can be turned on without handing out shells.~~
   **Done.** Every connection proves a key, and an unknown one waits for a
   person. The gate is a type: the LAN listener will take an authenticator that
   the loopback path cannot construct.
3. ~~**`zest-app` attaching to a daemon** rather than owning a pty.~~ **Done.**
   The window is a client of this machine's daemon over the loopback socket,
   using the same protocol the phone will use. Closing it detaches; the shell
   keeps running.

And four things underneath them that reading the code turned up. Each is
recorded here because none is visible from the feature list, and the third is
the one that decides whether M3's win condition is worth having:

- ~~**There is no `Delta` → `Terminal` applier.**~~ **Done.** `RemoteWriter` in
  `zest-core` and `Applier` in `zest-proto`, checked cell-for-cell against the
  host across five recordings and 10,000 random disconnect points.
- ~~**`DeltaOp::AltScreen` is emitted after `DeltaOp::Row`.**~~ **Fixed in
  protocol 2**, before anything could apply it wrongly. A screen switch now
  precedes the rows that describe it, guarded by
  `Delta::screen_switch_comes_first()` — the same shape as the existing
  `scrolls_come_first()`, asserted rather than sorted at runtime because a
  producer emitting them out of order has a bug that reordering would hide.
- ~~**Terminal modes never cross the wire.**~~ **Fixed in protocol 2.**
  `DeltaOp::Modes` and `Keyframe.modes` now carry `Modes::bits()`, so an
  attached client can encode its own keystrokes correctly. Verified over a real
  socket: `APP_CURSOR`, `BRACKETED_PASTE` and `MOUSE_CLICK` reach the client.
- ~~**The daemon unlinks its socket before binding.**~~ **Fixed.** An
  advisory lock on `<socket>.lock` is taken *before* anything is unlinked, so
  the second daemon refuses to start rather than stealing the path — and the
  stale-socket case becomes checked rather than assumed, since a lock that can
  be taken proves no live daemon holds it. Windows already had this via
  `FILE_FLAG_FIRST_PIPE_INSTANCE`.

---

# Workstreams

These were built in parallel, one worktree and one owner each. **That is over:
one lead, one lane, sequential commits on `main`.** The streams stay as *names
for bodies of work* — they make a commit's subject legible and each still has an
issue worth reading — but they are no longer ownership boundaries, and nothing
below means "do not touch this file".

| | Stream | About | Status | Issue |
|---|---|---|---|---|
| **A** | [Windows chrome, motion, polish](#ws-a) | `zest-app/src/{chrome,motion,platform}*`, `zest-render-wgpu/` | Open — closes M1 | [#5](https://github.com/zesterm/zesterm/issues/5) |
| **B** | [`zest-input`](#ws-b) | `crates/zest-input/` | Extracted ✅ · IME ✅ · Kitty open | [#2](https://github.com/zesterm/zesterm/issues/2) |
| **C** | [Unix PTY + macOS host](#ws-c) | `zest-pty/src/unix.rs`, macOS platform | C1 ✅ · **C2 in progress** — the app must run on the Mac to verify M3 there | [#3](https://github.com/zesterm/zesterm/issues/3) |
| **D** | [Linux host](#ws-d) | Linux platform + packaging | Open — C1 landed `unix.rs` | [#9](https://github.com/zesterm/zesterm/issues/9) |
| **E** | [Command blocks](#ws-e) | `zest-core/src/blocks.rs`, OSC 133, shell integration | Open | [#6](https://github.com/zesterm/zesterm/issues/6) |
| **F** | [`zest-proto` + `zest-daemon`](#ws-f) | `crates/zest-proto/`, `crates/zest-daemon/` | Protocol + daemon ✅ · **applier, app attach, LAN listener next** | [#4](https://github.com/zesterm/zesterm/issues/4) |
| **G** | [Web client](#ws-g) | `clients/web/`, `zest-proto/fixtures/` | Decoder + fixtures ✅ · renderer next, transport blocked | [#8](https://github.com/zesterm/zesterm/issues/8) |
| **H** | [Mesh identity, discovery, transports](#ws-h) | `crates/zest-mesh/`, `cloud/` | Identity + discovery ✅ · **pairing next** | [#7](https://github.com/zesterm/zesterm/issues/7) |

**Ordering that mattered, and is now settled.** B landed before A, so `zest-app`
is free of input code and A can fill it with chrome. C1 landed before D, so
`unix.rs` exists for Linux to build on. H's identity landed independently of F,
as planned.

**The sequencing rule, now discharged.** `listen_lan` was not to be turned on
until pairing existed. Both landed, and the ordering is enforced by the types
rather than by discipline: `LanListener::serve_forever` takes an
`Authenticator` by value, and `Auth::Transport` — the variant that skips the
trust store — can only be constructed by the loopback listener.

**The order M3 is being built in**, and why: the client half first (the delta
applier, then `zest-app` attaching to its own daemon over loopback), because it
is verifiable on one machine and it makes the daemon the everyday path. Pairing
and the LAN listener come after, so switching the LAN on enables something that
has been running in the dev loop for days rather than something never used.

**C2 was going to trail indefinitely** — the Mac only has to host, and the
Windows box renders. It has been pulled forward to a minimum slice for one
reason: development moved to the Mac, and an app that will not launch here means
the attach path ships compiled and unseen.

### WS-A — Windows chrome, motion, polish

M1 steps 11–13. Owns `zest-app/src/chrome/`, `motion/`, `platform.rs`, and
`zest-render-wgpu/`. Consumes `SessionSource`.

- [ ] **The fleet has no face on the desktop.** → [#23](https://github.com/zesterm/zesterm/issues/23).
      The phone and the web client are both planned to list sessions and attach
      to a chosen one; the app most people will use can only take a `--attach
      <host:port>` on the command line and then guess. The tab strip below is
      *window chrome* and answers none of it: what a tab is when sessions live on
      four machines, how loudly a remote one announces which machine it is, what
      a tab does when its host sleeps, and what opening a window should do at
      all. That last question already produced a bug — opening zesterm adopted a
      shell another machine was driving, because a default was standing in for a
      feature that does not exist.
- [ ] Borderless window, GPU-drawn titlebar and tab strip through the SDF rect
      pipeline. `WM_NCCALCSIZE` returning 0 with `top` untouched removes the
      caption while keeping frame, shadow and snap — **but when maximized you
      must also inset `top`** by `SM_CYSIZEFRAME + SM_CXPADDEDBORDER` or the tab
      bar hangs off the monitor. `HTMAXBUTTON` over the maximize rect is what
      enables Snap Layouts, and it suppresses ordinary mouse messages over that
      rect, so hover comes from `WM_NCMOUSEMOVE`.
- [ ] `ChromeHitMap` produced by the layout pass and consumed by **both** the
      renderer and the input path, so visuals and hit regions cannot drift.
- [ ] Animation clock. Springs `(response, damping)`, not easing curves —
      terminal motion is interruption-dominated and a spring absorbs a changed
      target with continuous velocity for free. Substep the integrator
      (`h = dt/ceil(dt·240)`) or a spring tuned at 60Hz behaves differently at
      144Hz. One clock, shared.
- [ ] Smooth scroll as a fractional row offset, **suppressed in the alt screen**.
- [ ] `reduce_motion`, honouring `SPI_GETCLIENTAREAANIMATION`.
- [ ] Per-OS backdrop: Mica via `DWMWA_SYSTEMBACKDROP_TYPE`.
- [ ] Polish: OSC 0/2 title, DECSCUSR cursor styles, font zoom, DPI changes.
- [ ] Validate gamma side-by-side against Windows Terminal. **Do not defer** —
      it ships broken constantly and reads as "looks slightly off".
- [ ] Perf validation: vtebench, >500 MB/s, <2ms CPU frame, <10ms keypress→pixel.

✅ **Every animator provably settles** — assert zero frames 250ms after the last
input. An animator that asymptotically approaches its target burns GPU forever at
0.01px/frame, and that is how the 0%-idle guarantee is lost.

### WS-B — `zest-input`

Extraction from `zest-app` collides with WS-A, so it landed early and small.

- [x] Key and mouse encoding live in the crate; `zest-app` holds none of it.
- [x] Super/Command belongs to the desktop, never the terminal, and copy/paste
      accepts both the `Ctrl+Shift` and Command conventions. Without this every
      Cmd chord on macOS typed its own letter — `Cmd+V` inserted a `v`.
- [x] IME and dead keys via winit `Ime`. `set_ime_allowed(true)` is what
      delivers the events at all, and on macOS it is also what makes dead-key
      sequences combine — without it `Option+e` `e` produces `e`, not `é`. The
      **preedit is drawn over the cursor and never enters the grid**: a
      composition belongs to the keyboard in front of one person, while the grid
      is shared with the daemon and with every other device attached to the same
      session, so writing provisional text into it would put half-typed
      characters into someone else's scrollback. Only the commit reaches the pty,
      as plain UTF-8 — not bracketed, because this is typing, not a paste.
- [ ] Kitty keyboard protocol (CSI u) behind a mode flag.

### WS-C — Unix PTY + macOS host

**Critical path for M3.** `PtyTransport` is already frozen, so this drops in.

**C1 — reachable** ✅ *(landed in #10)*
- [x] `zest-pty/src/unix.rs`: `openpt` + `Command` with `setsid`/`TIOCSCTTY` in
      `pre_exec`, resize via `TIOCSWINSZ`. `#[cfg(unix)] pub use unix::UnixPty as
      NativePty`. No shutdown protocol is needed — closing the master is a
      complete, non-blocking teardown, so ConPTY's ordering constraint has no
      unix counterpart.
- [x] `zest-pty`'s `terminal_env()` on unix — `TERM`, `COLORTERM`,
      `TERM_PROGRAM`, and clearing inherited terminal identity (Terminal.app,
      iTerm2 including the ssh-forwarded `LC_TERMINAL`, kitty, Alacritty,
      WezTerm, Ghostty, VTE).
- [x] `cargo test -p zest-core -p zest-pty` green on macOS.
- [x] The `headless` example runs a real `zsh` on macOS and prints its prompt.
- [x] Colour survives to the cells: `Indexed` and `Rgb` backgrounds both.
- [x] A macOS `vim` capture recorded to the corpus and replaying through
      `zest-core` (`crates/zest-core/tests/corpus/vim-macos.vtrec`).
- [x] Exit is clean — no hang, no zombie.

✅ **The Mac can host.** With WS-F, its shells appear on Windows.

**C2 — the GPU app on macOS**

No longer trailing. Development moved to the Mac, and every stage of M3's client
half ends in "look at the window" — an app that will not launch here means the
attach path ships compiled and unseen. The slice below is *runs and is usable*,
not *polished*; the rest of C2 can still trail.

- [x] **The app runs.** Metal on an Apple M4, 119×27, a real `zsh`, clean exit
      on `^D` with no stray processes. **`zest-app` needed no code changes at
      all** — winit and wgpu already do the right thing, and the only thing
      standing in the way was the font stack.
- [x] Font fallback verified through `font_dump` **before** the renderer, which
      is what it is for: it found three bugs in one run that would each have
      looked like a broken renderer. See the commit — the short version is that
      a CSS generic is not a family name, that the `system` font tests skip
      themselves when nothing resolves, and that box drawing is the third `Zyyy`
      case after emoji and the PUA.
- [x] Verified by eye in the window: bold, truecolor, background colour, box
      drawing, block elements, arrows, wide CJK, Greek, colour emoji and Nerd
      Font prompt icons.
- [x] `--startup-probe` reports **48ms** here. It measures a different thing
      than on Windows — different compositor, and none of the class-background
      brush that bought the 35ms — so **the 100ms budget stays a Windows
      assertion** and this number is reported, not gated.
- [ ] **Do not go borderless on macOS.** It costs traffic lights, native
      full-screen, Sequoia tiling and accessibility, and gains nothing over
      `titlebar_transparent` + `title_hidden` + `fullsize_content_view`. The
      traffic-light inset is not a constant — recompute on full-screen changes.
      **Deliberately not done yet:** a transparent full-size titlebar with
      nothing drawn into it is an empty strip, not an improvement. It wants to
      land with WS-A's chrome, which is what fills it.
- [ ] `NSVisualEffectView`, and the rest of the polish — still trailing.

### WS-D — Linux host

Shares `unix.rs` with WS-C; owns only Linux-specific parts.

- [ ] Vulkan surface, fontconfig fallback verification.
- [ ] Negotiate `zxdg_toplevel_decoration_v1` or KDE gives you *two* titlebars.
- [ ] Transparency via an ARGB visual. **Blur has no portable path** — X11/KWin
      has `_KDE_NET_WM_BLUR_BEHIND_REGION`, picom needs user rules, Wayland has
      no protocol. Degrade honestly rather than pretending in the settings UI.
- [ ] Packaging.

### WS-E — Command blocks

M2. Owns `zest-core/src/blocks.rs` and the OSC 133 path. Hot spot: coordinate
`perform.rs` edits, which WS-F also reads.

- [x] **OSC 133 A/B/C/D, OSC 7 (cwd), OSC 633 (VS Code alias) → `BlockIndex`.**
      The index lives on `TermState`, so `Terminal::blocks()` answers for a
      local session and — once the wire carries them — for one running on
      another machine, through the same accessor.

      Four things that each produce a plausible-looking wrong index, and are
      therefore tested rather than assumed: **the alternate screen is a
      separate grid whose ids restart at zero**, so markers there are ignored
      outright; **`133;D` with no status is `None`, never `Some(0)`**, because a
      green tick on a command that failed is worse than no tick; **`Grid::reflow`
      returns a `Reindex`** and blocks re-anchor through it; and **eviction is
      wired**, guarded so the common case is one comparison rather than a scan
      of the index per line of output.

      The command text is read back from the grid between `B` and `C` — the
      markers carry positions, not text — except under OSC 633, whose `E`
      states it outright and is preferred.
- [x] **Blocks on the wire.** Not optional: the window is a client of its own
      daemon, so a block parsed host-side was invisible until `zest-proto`
      carried it. Additive — `Delta.blocks` and `Keyframe.blocks`, both
      `serde(default)` — so no protocol bump. A *field* rather than a `DeltaOp`
      variant, because a tagged enum's unknown variant fails the whole `Delta`.
      Verified over a real socket with a real `zsh`: `exit 0`, `exit 1` and a
      running command with no end line all arrive intact, along with the cwd
      from OSC 7 (`cargo run -p zest-daemon --example attach`).
- [x] **Shell integration for `zsh`.** Env-only injection, as kitty, Ghostty
      and VS Code all do it: `ZDOTDIR` points at a generated shim that hands
      control straight back to the user's own dotfiles, then loads the hook
      *after* their `.zshrc` — after, because `add-zsh-hook` appends and a
      prompt framework rebuilds `PS1` in its own `precmd`. **No file of the
      user's is written.** Not "consented auto-install", which is what this
      said before and is iTerm2's older model; kitty states the difference
      outright — *"No files are added or modified."*

      `A`/`B` live inside `PS1` wrapped in `%{...%}`, not in `precmd`: printed
      from `precmd`, `A` lands on the line before the prompt and `B` cannot be
      placed at all — and unwrapped, zsh miscounts the prompt width and
      mispositions the cursor on every redraw, which looks like a rendering bug.

      Verified against a real interactive `zsh` with the author's own `.zshrc`:
      `echo hello` → `D;0`, `false` → `D;1`, cwd from OSC 7. The recording is in
      the corpus as `blocks-zsh.vtrec`, which is what makes the conformance
      block assertions non-vacuous.

      `zesterm --shell-integration zsh` prints the same hook to `eval` by hand —
      the documented path for ssh, tmux and subshells, which injection
      structurally cannot reach. `zest-daemon --no-shell-integration` turns it
      off.
- [ ] **bash, fish and PowerShell.** Deliberately not written yet: none can be
      *seen working* on the machine this is built on. There is no fish and no
      pwsh here, and `/bin/bash` is 3.2.57 — Apple's patched build, where the
      `ENV` startup path injection depends on is disabled, and which Ghostty
      excludes on Darwin outright for that reason. Writing them blind is how the
      attach path nearly shipped compiled and unseen.
- [ ] **A settings key for shell integration.** Today it is a daemon flag, which
      is not where anyone will look. The shell runs on the *host*, so the host
      decides — but `zest-daemon` has no settings reader, since it does not
      depend on `zest-config`. Closing that means either the dependency or a new
      field on the frozen `CreateSession`, and neither is worth doing before
      someone wants the switch.
- [ ] **The `/etc/zshenv` hole.** A system `zshenv` that re-sets `ZDOTDIR` runs
      *after* our environment and silently undoes the injection — Ghostty
      documents having no fix, kitty tracks it as #6330. This Mac has no such
      override, so the failure is currently untested rather than handled; it
      wants detecting and reporting rather than looking like a shell that
      emits no markers.
- [x] **Copy-output and re-run**, keyboard and mouse, on the same chord as copy
      and paste (`Cmd` / `Ctrl+Shift`): `O` copies what the last command
      printed — not its prompt, not the command — and `R` runs it again. The
      chord plus a click does the same for a specific block anywhere in
      scrollback, which is the thing a keyboard shortcut cannot express.

      Both target the most recent block *with output*, not the block the cursor
      is in: at a prompt the cursor's block has printed nothing, which is the
      state a terminal spends most of its life in.

      Writing this found a real bug in the markers. `133;C` fires before the
      shell echoes the newline and `133;D` after the trailing one, so
      `output_line` was landing on the command's own row and `end_line` on the
      *next* prompt's. Taken literally, copy-output returns a prompt at each end
      of what it copied, and folding hides one line too many. Both are adjusted
      in the parser rather than in each consumer, so the wire and the phone get
      the corrected meaning too.
- [ ] Block folding. The one part that needs a renderer change: `Viewport` would
      carry the ranges to skip, and the row loop compact over them. Selection
      coordinates, mouse row→line mapping and the scroll maths all read that
      same row list, which is why it is its own step rather than a rider on the
      actions above.

      Still **not blocked on WS-A**, which is worth stating because it looks as
      though it should be.
      The renderer already has the rect pipeline, `Chrome { rects, glyphs }` and
      absolute-pixel `GlyphInstance`s, so block headers have somewhere to be
      drawn; and the actions themselves reuse what exists — `block_at` for
      folding, `Selection` + `selection_text` for copy-output, and
      `ClientMessage::Input` for re-run, which needs no new protocol type.

      Only *pointing* needs WS-A: `ChromeHitMap` is the seam that lets a click
      land on a fold triangle. Keyboard-driven block actions need none of it, so
      the sensible split is keyboard first, clickable when WS-A lands. The one
      genuine renderer change is folding — skipping folded rows while building
      the viewport, which is `zest-app`'s grid extract rather than chrome.

> The strongest reason this project owns its grid rather than depending on
> `alacritty_terminal`: blocks need new row fields, a side index surviving
> scrollback eviction, and OSC handler hooks. Each would be a fork of an
> explicitly-unstable crate.

### WS-F — `zest-proto` + `zest-daemon`

**The lead stream.** Wire types are frozen; encoding and the daemon are not.

- [x] `ChangeSource` on `Terminal` — `zest-core/src/subscribe.rs:108`, consumed
      by `zest-daemon/src/session.rs`. The `update_for` rule is settled and
      tested.
- [x] Delta encoder: attribute interning, `SCROLL` before `ROW`, explicit cell
      counts. MessagePack envelope; individual cells are **not** msgpacked.
- [x] `zest-daemon`: session ownership, loopback transport (unix socket and
      overlapped named pipe), protocol loop.
- [x] **The `Delta` → `Terminal` applier.** `zest-core`'s `RemoteWriter` is the
      one named door into a grid that is not the VT parser; `zest-proto`'s
      `Applier` drives it from the wire. `GridView` stays as the reference for
      TypeScript clients, and the two are checked against each other.
- [x] The daemon tells the truth about sequence. Every update names the state
      it builds on and the one it produces, in an unbroken chain; `Ack` is
      recorded as a *separate* number from what was sent, because advancing one
      counter on send is the host asserting that everything it wrote was
      applied. `RequestScrollback` is answered from `Grid::lines_by_id`, with
      the attributes those rows name.
- [x] `zest-app` gains a daemon-attached `SessionSource`, in the slot the shell
      spawn occupied — after the window is visible, overlapping driver init.
      `--startup-probe` still passes (39ms on the Mac). Measured with
      `--attach-probe`: **0.02ms to connect warm**, ~1ms to the first keyframe,
      ~7ms when it has to start the daemon. `--no-daemon` keeps the pty
      in-process, and so does any failure to reach a daemon.
- [x] **Conformance corpus**: the `.vtrec` replay now has three participants —
      the host `Terminal`, `GridView`, and a client `Terminal` fed by the
      applier — and asserts **two real `Terminal`s are cell-for-cell equal at
      every frame**, with exactly one exclusion, named in the failure message so
      nobody widens it quietly. This is the spine.
- [x] Chaos-resync 10,000 times at random disconnect points, from three seeds,
      in under a second — so it runs on every `cargo test` rather than behind
      `--ignored`, where CI would never see it. The stale-`base` path is
      exercised on every iteration, not once in a fixture.
- [x] `ts-rs` codegen and golden-fixture contract tests in CI. The bindings are
      committed under `crates/zest-proto/bindings/` and gated by
      `check-bindings`; the corpus is exported to `crates/zest-proto/fixtures/`
      and gated by `check-fixtures`. The two catch different things — a shape
      that moved, and a client that decodes the right shapes and applies them
      wrongly — and only the second needs real recordings.
- [x] **A first pairing survives long enough to be answered.** The handshake
      watchdog was disarmed only by `welcome()`, and a device waiting for
      approval is precisely the one that has not been welcomed — so every LAN
      pairing was cut ten seconds into a window the host advertises as 120, and
      approval-based pairing had never once worked. The watchdog's *deadline*
      now moves out past the pairing window rather than the watchdog being
      disarmed: the connection is still unauthenticated and still holds its
      mid-handshake slot, so nobody can pin slots open by asking to pair and
      never being answered. Found by two machines in one attempt; the
      in-process test that was supposed to cover it proved the easy half.
- [x] **A session is not collected before the client that made it can attach.**
      Creating a session and attaching to it are two round trips, and a short
      command exits in between — so the sweep took the session inside the gap
      and the client was told no such session existed, for a shell that had run
      perfectly. Sweeping now also requires that somebody has attached at least
      once.
- [x] **Sessions end when they should, and only then.** `CloseSession` now ends
      its child — it used to remove the registry entry and drop the transport,
      which on unix cannot hang up a pty whose reader is parked, and on Windows
      only works if that `Arc` happened to be the last. A shell that exits on
      its own is collected once nobody is watching, rather than being reported
      as exited and then kept, with its scrollback, forever. And a connection
      that vanishes releases its subscriptions, which a polite `Detach` did but
      a closed lid did not. → `PtyTransport::hangup`, a deliberate contract
      change; see CONTRACTS.md.
- [ ] **Assert client scrollback equals the host's.** `SbPush` is emitted only
      when the encoder calls a viewport move a scroll, and a jump larger than the
      viewport deliberately is not one — so the host pushes history the client is
      never told about. Nothing checks this, which is why the fixtures carry no
      scrollback expectation: it would pin a divergence rather than catch it.
- [ ] **The corpus has three holes**, found by exporting it. No recording
      contains a combining mark, so `conformance.rs` dropping its marks exclusion
      after `4b3152e` proved less than it looks — the real coverage is
      `apply.rs`'s unit tests. Nothing reaches past the BMP, so every wide
      character in it is CJK. And at the natural sizes only `vim-macos` scrolls
      enough to exercise `SCROLL` ordering. The fixtures cover all three
      synthetically and guard against regressing; **recorded sessions would be
      better**, and `conformance.rs` would benefit from the same three.
- [x] **The window reconnects.** A dropped link used to be terminal: the
      session kept running in the daemon exactly as ADR-007 promises, and the
      window could never reach it again. It now retries with a bounded backoff
      and **adopts** rather than creates, so the shell that was lost is the one
      picked up — which works because a dropped connection releases its
      subscriber, leaving that session unattached. Verified by killing the
      daemon under a live window and restarting it.
- [x] **The whole workspace passes on a real Windows machine** — issue #18's
      baseline, and a first. What stood in the way was a hang, not a failure:
      `tests/lan.rs` asked for `/bin/echo`, the Windows spawn failure came back
      as an `Error` frame its `wait_for` filter ignored, and a blocking read
      with no timeout made the 10s deadline decorative — so the suite parked
      forever. Masked until `5bfff82`, because the lib tests failing stopped
      `cargo test` before the lan binary ever ran; the process watcher took the
      blame for the first hung CI run, but the two runs after its backout hung
      the same way. `wait_for` now sets a read timeout, so this class of bug is
      red in ten seconds instead of cancelled after an hour.
- [x] **Reconnect happens in place.** `RemoteSession` supervises its own link:
      it redials, re-proves its key, and reattaches to *the same session*,
      applying the keyframe into the `Terminal` already on screen — so the
      client's scrollback survives a dropped link rather than being rebuilt from
      nothing. Input typed while disconnected is dropped and only the newest
      resize is kept, because replaying thirty seconds of queued keystrokes is
      how a reconnect runs a command the user abandoned.
- [x] **The attach example dials TCP** (`--addr <host:port>`), one generic loop
      over both transports — found missing at the exact moment the two-machine
      bring-up (#20) needed its step 2: pairing was done and no tool existed
      between "trusted" and "a window". Throwaway identity, so an unpaired host
      prompts and the code ritual applies; proven end to end over TCP against a
      live daemon before first use on the real LAN.
- [x] **The window can attach to another machine's daemon**: `zesterm --attach
      <host:port>`. A TCP dialer into the same `RemoteSession` the loopback
      path uses — the whole point of that abstraction, cashed in. Remote
      attach **fails loudly instead of falling back** to an in-process pty: a
      user who asked for a specific machine and silently got a local shell has
      a window that looks right and lies. Identity is still ephemeral per
      launch, so the far host prompts each time; a stored identity (and the
      keychain prompt it drags onto this path) is deliberately future work.
- [ ] SQLite scrollback. Scrollback is in memory and bounded; a session that
      outlives its window does not yet outlive the daemon.

### WS-G — Web client

`clients/web/`, plus the Rust that generates what it is checked against — the
exporter cannot live in TypeScript, and "no path ownership" is the rule now
anyway.

The decoder was built before the daemon can be reached at all, which was the
point: **a browser cannot open a unix socket or raw TCP, and the daemon speaks
nothing else.** ADR-005 names the data plane as a binary WebSocket; nothing
implements it. Everything below the first item waits on that.

- [x] TypeScript delta decoder against the `ts-rs` bindings, replaying the
      conformance corpus frame by frame. `cargo xtask fixtures` exports
      `crates/zest-proto/fixtures/`; `check-fixtures` gates it; the TypeScript
      compares against the **host `Terminal`'s** own cells rather than against
      `GridView`, or two implementations wrong in the same way would agree.

      41 tests over 13 fixtures — 82k cells. No runtime dependencies: framing,
      MessagePack and the decoder are hand-written, which is affordable because
      44 recorded frames prove them on the first run. Two gates that catch
      different things: `pnpm -r typecheck` catches a wire *shape* that moved
      (`bindings-match.test.ts` is a type-level test, so `tsc` is what evaluates
      it), and `pnpm -r test` catches applying the right shapes wrongly. Each
      was verified by breaking it.

      Three coverage holes the corpus turned out to have, each now closed by a
      synthetic fixture and a guard that refuses to regenerate without it: no
      recording contains a combining mark; **nothing anywhere reaches past the
      BMP**, so the UTF-16-versus-code-point trap — the single most
      JavaScript-specific bug available here — was invisible; and at the natural
      viewport sizes only `vim-macos` scrolled enough for `scroll`-before-`row`
      to matter, so the ordering invariant had one fixture behind it and now has
      three.
- [ ] Grid renderer. **`@sigx/terminal` cannot be reused** — it paints TSX *to* a
      TTY, which is the inverse of what a web client needs.

      **Canvas 2D**, behind a "given a grid and its dirty rows, paint" seam.
      Deltas already name the rows that changed, so repaints are row-scoped and
      the usual reason to reach for WebGL never arises; `fillText` inherits the
      browser's font fallback, colour emoji and PUA icons, which is the `Zyyy`
      trap this project has already paid for once; and WebGL would share no code
      with `zest-render-wgpu` without a wasm crate. Swap in an atlas backend on
      measurement — a large grid repainting most rows below 60fps — not on
      instinct.
- [ ] SignalX app: session list, attach, input.
- [ ] Local echo prediction for high-latency links (mosh's other trick): predict
      printable-char echo when not in alt-screen, render dim, reconcile on delta
      arrival. The largest perceived-latency win available.

### WS-H — Mesh identity, discovery, transports

- [x] Ed25519 host and client keypairs; `HostId` **is** the public key, since
      both are 32 bytes and a fingerprint of something already that short only
      buys a second field to carry the key in. Private keys through Keychain /
      Credential Manager / Secret Service, never a file — a machine with no
      store refuses to start rather than writing one.
- [x] mDNS discovery implementing `Discovery`. **M3 needs only this.** The wire
      format and the roster are pure functions, so every rule is tested without
      a socket; `--ignored` covers the rest. `LayeredDiscovery` merges
      `StaticDiscovery` with mDNS by `HostId`, which is where "the LAN beats the
      tunnel" actually happens.
- [x] Pairing: a signed transcript on every connection, a matching code, and an
      approval queue. A stolen session alone does not get a shell. The desktop
      modal is not built — the daemon prompts on stdin and `--trust` covers a
      headless host — but it is a front-end swap over the same queue, and the
      wire messages for it already exist.
- [x] **A killed daemon says goodbye** (#22, polite half). A signal runs no
      destructors, so pkill and Ctrl+C used to leave the host on every fleet
      listing for the PTR record's 75-minute TTL — four stale rows in one
      afternoon of #20. A `ctrlc` handler withdraws the advertisement on the
      way down; verified live on Windows, listing flipped online→away in one
      probe tick. SIGKILL/`Stop-Process`/crashes still leak the record —
      mdns-sd exposes no TTL control — so #22 stays open for
      reachability-as-presence in the fleet listing.
- [x] **Reachability is presence** (#22, impolite half). SIGKILL and crashes
      send no goodbye and mDNS caches keep the record for 75 minutes; only a
      dial can see through it. `Presence::Unreachable` — advertising, port
      refuses — fed by `Roster::report_dial`: a failed dial marks it, and only
      a successful dial or a *changed* advertisement clears it, because a
      cache renewing an identical record is the same claim repeated, not
      evidence. The roster stays socket-free; `mesh_probe` carries the actual
      prober (10s interval, so a listing is at most ten seconds wrong).
      Verified live: a `TerminateProcess`'d daemon flipped to UNREACHABLE in
      one interval and stayed there through 47s of cache re-announcements —
      and a probed live daemon logs nothing, after the handshake watchdog
      learned to warn only when it cuts a connection that still exists.
- [ ] Cloudflare Tunnel + Access per host. **Origin-side JWT validation is
      mandatory** — the origin never trusts the tunnel.
- [ ] The directory Worker: host ids, labels, last-seen endpoints. **No session
      state, never in the data path.** → ADR-006.
- [ ] Remote access **off by default**, persistent indicator, audit log.

---

# Milestones

## M1 — a terminal worth using on Windows

**Win condition:** *"I used this instead of Windows Terminal for a week and
didn't want to switch back."* Not feature parity with WezTerm.

- [x] **0. Toolchain.** VS Build Tools. The Windows SDK alone is not enough —
      `libstd` needs the MSVC CRT for `__CxxFrameHandler3` and `__chkstk`.
- [x] **1. Workspace + enforced boundaries.** `cargo xtask check-deps`, 7
      boundaries.
- [x] **1.5. Transparency probe.** Settled premultiplied alpha before any shader
      existed. → ADR-003.
- [x] **2. `zest-pty`.** ConPTY with the shutdown protocol, `.vtrec` recording,
      and a terminal identity for the child.
- [x] **3. `zest-core`.** Ring storage, 16-byte `Cell`, deferred wrap, scroll
      regions, alt screen, CSI/SGR/OSC, absolute line IDs, sequence counter.
- [x] **4. `zest-font`.** swash + fontique, system fallback, COLR/CBDT colour
      glyphs, Nerd Font / PUA icons. PNG dump for diagnosis.
- [x] **4b. `zest-theme`.** Five built-ins, OKLCH `ui.*` derivation, four
      importers.
- [x] **5. `zest-render-wgpu`.** Three pipelines, one pass, `Rgba16Float`
      offscreen + resolve, offscreen PNG harness with `--replay`.
- [x] **6. `zest-app`.** winit, PTY thread, fair mutex. **A working terminal.**
- [ ] **7. `zest-input`.** → WS-B.
- [x] **8. Selection + clipboard.** Absolute `LineId`, word/line/block modes,
      wrapped-line copy, bracketed paste.
- [x] **9. Scrollback + scrolling.** Wheel, alt-screen wheel→arrows,
      Shift+PgUp/PgDn, `scroll_on_output`.
- [x] **10. `zest-config`.** Cascade with provenance, profiles, migrations, hot
      reload with invalidation classes, JSON Schema export, workspace trust.
- [ ] **11–13. Chrome, motion, polish, perf.** → WS-A.
- [x] **12b. Startup latency.** Window at ~43ms, prompt on the first frame. Was
      ~1.9s of white.

      The fix was to stop waiting for the GPU: a window does not need one to be
      the right colour, so a class background brush lets Windows paint it
      immediately. Spawning the shell *before* GPU init overlaps pwsh's ~400ms
      with the driver's ~850ms.

      Remaining cost before content, on an RTX 3080 Ti / Vulkan: ~350ms Vulkan
      instance, ~120ms device, ~290ms swapchain, **~245ms naga WGSL→SPIR-V**,
      ~90ms pipelines with the cache warm.

      Two things that did **not** help, recorded so they are not retried:
      merging shader modules (the cost is translation, not per-module overhead),
      and preferring DX12 (it redistributes driver init and totals the same).

      Next lever is the 245ms: precompile WGSL to SPIR-V at build time with
      `SPIRV_SHADER_PASSTHROUGH`. Vulkan-only and `unsafe`, needs a WGSL
      fallback.

## M2 — command blocks

→ WS-E.

## M3 — the fleet, on your LAN

**Win condition:** *"My Mac's shell, in a window on my Windows box, at desk
latency."*

**Closed, 2026-08-08.** Both lanes concur; the bring-up is logged
blow-by-blow on [#20](https://github.com/zesterm/zesterm/issues/20).

Everything the milestone asked for is not merely built but *observed on two
machines*: pairing in both directions with the six digits compared by two people,
stored device identities on both platforms, trust that survives a daemon restart,
a live window riding through that restart unprompted, and a number for desk
latency. What the second machine cost, and repaid, is the honest headline —
**six bugs, none of them findable from one box, and every in-process test had
agreed each was fine:**

| | |
|---|---|
| the handshake watchdog cut connections that were **waiting for a person**, so approval-based pairing had never once worked | mine |
| a stale line on the approver's stdin **approved a device nobody looked at** | mine |
| the sweep collected a session **before the client that made it could attach** | mine |
| `zesterm` and `attach` had **no TCP transport at all** — the win condition had no path | shared |
| a client that lost its link **could never come back** | mine |
| a killed daemon **kept advertising**, so a peer saw a host it could not dial | shared |

The pattern in the four that were mine is worth more than the count: each had a
passing test that proved the *easy half*. The watchdog test used a device that was
already trusted — the one case that never goes through approval. That is what a
second machine buys, and no amount of care on one buys it.

**Met, 2026-08-08.** `zesterm --attach <mac>:7717` on the Windows box, pairing
approved with the six-digit code compared on both machines, and
`andii@Andreass-MacBook-Air main %` in a GPU-rendered window — `uname -a`
typed on one machine and answered by the other's kernel. The whole bring-up,
including the three bugs it found (a watchdog cutting connections that were
waiting for a person, swallowed approver answers (#21), and the attach example
having no TCP transport at all), is logged blow-by-blow on
[#20](https://github.com/zesterm/zesterm/issues/20). Remaining before M3 is *closed*: the **host** half of stored identities, and
the LAN half of the latency number.

**The client half is done.** A window attaching to a remote host now uses a
*stored* client key rather than a throwaway one, so a person approves it once
instead of once per launch:

```
launch 1   remote_attach_ms=2871.06   (approved by a human)
launch 2   remote_attach_ms=1.47      (no prompt)
launch 3   remote_attach_ms=2.03      (no prompt)
```

The keychain is answerable here in a way it is not for the daemon — this is a GUI
process with a user in front of it, while the daemon is detached and blocks on a
prompt nobody can see. That asymmetry is why the client half could land and the
host half cannot yet: a daemon with a *persistent* identity and a file trust store
needs an answer to the detached-prompt problem first. Until then `--ephemeral`
means every daemon restart re-pairs every device.

**Desk latency has a number.** Windows → Mac over Wi-Fi, keystroke to the delta
carrying its echo, 200 round trips: **p50 6.8ms, p99 12.6ms**, min 5.6ms. Under
half a 60Hz frame, so type-to-paint on the far machine lands within one frame of
the local case. That is a property of the link, not the protocol — the protocol's
share is below.

**ADR-007's 50–100µs is measured, and it holds.** `attach --ping 300` on
loopback, keystroke bytes on the wire to the delta carrying their echo:

| | warm | first run |
|---|---|---|
| p50 | **11µs** | 25µs |
| p99 | **20µs** | 77µs |
| max | 35µs | 132µs |

Comfortably inside the claim, and the first run is slower for the ordinary
reason — nothing is warm yet. This is a **floor** for input-to-paint, not the
number a person feels: it stops at the delta and never touches the renderer.

Two things the bring-up established that no test could have, both recorded so
they are not re-litigated:

- **mDNS crosses a real link, and the `HostId`-derived SRV target resolves.**
  Cross-checked against the platform's own responder rather than ours —
  `zesterm-<id>.local` answering with an address is what sharp edge 5 was
  written to guarantee, and it does.
- **Windows Defender did not block the inbound port.** No prompt, no rule, the
  first dial simply arrived. Worth stating because the opposite is the natural
  assumption and would send the next person configuring a firewall that was
  never in the way.

And two that it broke: a daemon killed rather than dropped keeps advertising, so
a peer sees a host it cannot dial ([#22](https://github.com/zesterm/zesterm/issues/22)),
and an approval written to the daemon's stdin can be swallowed
([#21](https://github.com/zesterm/zesterm/issues/21)) — which means "an unknown
device waits for a person" is, today, conditional on that person trying twice.

No Cloudflare, no identity infrastructure — which is what makes this reachable
much sooner than the old "phone over the internet" framing. → WS-C1, WS-F,
WS-H (discovery only).

**The crux: ship grid deltas, not raw PTY bytes.** Raw bytes (ttyd/wetty) are
simpler and proven, but lose on four counts — resync is unsolvable on a
reconnecting link, deltas coalesce and bytes don't, blocks are semantic rather
than textual, and two VT emulators means two truths. → ADR-004.

## M4 — the fleet, anywhere

Cloudflare Tunnel + Access per host, device enrollment, the directory Worker,
the web client. → WS-G, WS-H.

**Actors are the control plane, never the data plane**, and they run **locally**
on each host. → ADR-005, ADR-006.

- [ ] Bun single-file sidecar hosting `@sigx/actors`, spawned as a child of the
      daemon, length-prefixed msgpack over stdio. Never in the PTY hot path.
- [ ] Device enrollment: non-extractable Ed25519 key, desktop approval modal
      with a matching code.
- [ ] Attach tickets (30s TTL, single use) minted by the actor.

## M5 — phone, AI, end-to-end encryption

- [ ] Lynx app. **Blocks-first, not grid-first** — a phone is excellent at
      lists, and you drop into grid view only when `alt_screen` is true, which
      the host already reports. Sticky `Ctrl` toggle, local history from the
      block index, long-press to re-run.
- [ ] **E2E encryption of the data plane** (Noise IK / HPKE, keys bound to
      device enrollment). The only mitigation that survives a hostile relay —
      first class, not a stretch goal. It converts Cloudflare from a trusted
      party into a dumb pipe.
- [ ] `AiActor` over sigx `streams:`, per-block consent, redaction.

---

## Dogfooding

zesterm must correctly host `@sigx/terminal` TUIs — alt-screen, truecolor, raw
mode, resize, cursor and erase. Use `examples/showcase` and
`examples/claude-shell` from `C:\Dev\sigx\terminal\main` as acceptance content.

Theme `ui.*` tokens are `@sigx/terminal-zero`'s contract verbatim, so one theme
file styles zesterm's chrome *and* any sigx TUI running inside it.

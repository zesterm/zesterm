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
        │   the Cloudflare relay (~60ms) when away
        │
   ┌────┴──────────┬──────────────┬───────────────┐
   │ zest-daemon   │ zest-daemon  │ zest-daemon   │  hosts
   │ (Windows)     │ (Mac)        │ (Linux)       │
   │  PTYs, grid, scrollback, command blocks      │
   └──────────────────────────────────────────────┘

   Cloudflare holds a *directory*: which hosts are yours, and are they up.
   Not how to reach them — under dial-back there is no address to hold, and
   the LAN finds its own over mDNS. No grid, no scrollback, no session
   state — ever.

   On the away path it also carries the bytes, and carries them blind: the
   relay is a pipe both ends dial out to, and everything after the Challenge
   is sealed end to end before it arrives. → ADR-008, ADR-009.
```

## Status

**692 tests, six gates green**, measured on macOS rather than remembered.
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
| `zest-input` | ✅ extracted; keys + SGR mouse + selection + IME + Kitty CSI u (flags 1, 2, 8), Rust and TypeScript — ⬜ Kitty flags 4/16, keypad |
| `zest-app` | ✅ window, tabs (top strip / left sidebar) behind `SessionSource`, **attached to its own daemon**, fleet picker (⌘K), restore-on-launch — runs on Windows *and* macOS (Metal, transparent titlebar) — ⬜ Windows chrome, motion |
| `zest-proto` | ✅ protocol 2, encoder, `Applier` into a real `Terminal`, `GridView` for TS clients, framing, cell-for-cell conformance, chaos-resync, command blocks |
| `zest-mesh` | ✅ Ed25519 identity, keystore, mDNS discovery, layered fleet, pairing + trust store, sealed channel |
| `zest-cloud` | ✅ the fence held in both directions: rustls (ring) landed here and `check-deps` stayed green with no list edited, and `zest-daemon`'s `--enroll` is now a real consumer — `TlsDuplex`, one connection as two independently owned halves, a one-request HTTP POST over it, `Endpoint` — and `zest-daemon`'s `--relay` is the second, dialling `TlsDuplex` per pipe under ADR-009's dial-back |
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
  OSC 133 hook through a `ZDOTDIR` shim that sources the user's own dotfiles and
  writes none of them; PowerShell gets it dot-sourced from the command line,
  having no `ZDOTDIR` to point anywhere, and keeps whatever `prompt` it already
  had. Either way blocks appear against the prompt the user already has, and no
  file of theirs is touched. VS Code's OSC 633 is understood too, for anyone who
  has its integration.
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
| **B** | [`zest-input`](#ws-b) | `crates/zest-input/` | Extracted ✅ · IME ✅ · Kitty CSI u ✅ · flags 4/16 open | [#2](https://github.com/zesterm/zesterm/issues/2) |
| **C** | [Unix PTY + macOS host](#ws-c) | `zest-pty/src/unix.rs`, macOS platform | C1 ✅ · **C2 in progress** — the app must run on the Mac to verify M3 there | [#3](https://github.com/zesterm/zesterm/issues/3) |
| **D** | [Linux host](#ws-d) | Linux platform + packaging | Open — C1 landed `unix.rs` | [#9](https://github.com/zesterm/zesterm/issues/9) |
| **E** | [Command blocks](#ws-e) | `zest-core/src/blocks.rs`, OSC 133, shell integration | Open | [#6](https://github.com/zesterm/zesterm/issues/6) |
| **F** | [`zest-proto` + `zest-daemon`](#ws-f) | `crates/zest-proto/`, `crates/zest-daemon/` | Protocol + daemon ✅ · **applier, app attach, LAN listener next** | [#4](https://github.com/zesterm/zesterm/issues/4) |
| **G** | [Web client](#ws-g) | `clients/web/`, `zest-proto/fixtures/` | Decoder, renderer, app, deploy, accounts, fleet, tabbed chrome ✅ · **devices screen, local echo next** | [#8](https://github.com/zesterm/zesterm/issues/8) |
| **H** | [Mesh identity, discovery, transports](#ws-h) | `crates/zest-mesh/`, `crates/zest-cloud/`, `cloud/` | Identity, discovery, pairing, accounts ✅ · the relay Worker and the daemon's `--relay` leg ✅ · **the web client's second data plane next** ([#59](https://github.com/zesterm/zesterm/issues/59)) | [#7](https://github.com/zesterm/zesterm/issues/7) |

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

**Visual target:** [docs/design/client-ui/](design/client-ui/README.md) — the
high-fidelity handoff for the tabbed chrome, command blocks, palette, fleet and
theme screens. Colours, sizes and spacing come from there, not from this file.

**Handoff v2 (2026-08-12, #127)** widened the reference to 12 screens: it adds
§11 (Settings as a tab) and §12 (Profiles — launch targets), a bundle
[AGENTS.md](design/client-ui/AGENTS.md) carrying the invariant checklist and
verification protocol, a runnable `zesterm-demo.html`, and `screenshots/` (one
rendered PNG per screen). It also *revises* screens 1–2 against what shipped,
deliberately: tab chips carry the title only, the `+` opens a profile launcher
menu, the status bar is deleted, and the layout toggle leaves the chrome
(`tabs.position` is the single source of truth; the ⌘⇧E chord and its palette
entry stay). The resulting work items, measurements in the handoff README — **all landed** (the native run of #149–#178; ADR-012 records the architectural rule they established):

- [x] `TabContent` — tabs that aren't sessions (Settings, Profiles), without
      touching the `SessionAddr`-keyed hit machinery. Settings landed with
      [#169](https://github.com/zesterm/zesterm/issues/169), Profiles with
      [#168](https://github.com/zesterm/zesterm/issues/168)/[#176](https://github.com/zesterm/zesterm/issues/176):
      each app tab lives behind a reserved `SessionAddr` on the all-zero
      host (`settings_addr()` = `u64::MAX`, `profiles_tab_addr()` =
      `u64::MAX - 1` — a test pins that the pair differs, ADR-012 records
      why), so every hit region and activation path stayed put.
- [x] Screens 1–2 reconciliation → [#149](https://github.com/zesterm/zesterm/issues/149):
      title-only chips (closes #51 structurally), status-bar deletion,
      layout-pill removal, full-width vertical header, horizontal strip scroll
      keeping the active tab in view. The *vertical* sidebar clamps its scroll
      but does not yet ensure-visible on activation — that gap is still open.
- [x] `--screen <fleet|themes|settings|palette|launcher|profiles>`, composing
      with `--screenshot`, so every design screen is capturable headlessly
      → [#161](https://github.com/zesterm/zesterm/issues/161). Each arm makes
      the exact call the keyboard would; `settings` opens the §11 tab (it
      opened the old overlay before #169, through the same call — the flag
      never changed). `launcher` and `profiles` were parsed-but-refused until
      #168 landed them (the one-line arms this entry predicted).
      `--tabs-position <top|left>`
      landed alongside it: a CommandLine-layer override like `--theme`, so
      both chip orientations (handoff README §§1–2) are capturable too.
- [x] The `+` launcher menu (README §1) →
      [#168](https://github.com/zesterm/zesterm/issues/168): clicking the `+`
      opens the menu (⌘T still spawns the default directly); rows come from
      `Settings::profiles` through `resolve_profile` (an empty table degrades
      to one synthetic default-shell row — the menu never renders empty), the
      default row leads and `⏎` runs it, plain digits 1–9 launch the Nth row,
      `⇧⏎` opens the fleet picker, and *Manage profiles* opens the singleton
      Profiles tab (⌘⇧,; a placeholder pane until §12's editor lands —
      `--screen launcher|profiles` now work). Launches seed the tab's palette
      through the #162 identity, so a profile's scheme shows on frame one.
      v1 launches on the window's route: a profile's `host` key (and its row
      chip, per the dead-affordance rule) waited for the cross-host item,
      which landed as #175 below.
- [x] Settings as a tab (README §11), replacing the ⌘, overlay →
      [#169](https://github.com/zesterm/zesterm/issues/169): ⌘, opens or
      activates the singleton tab (closing it is closing a tab — ⌘W, or Esc
      past the filter); `chrome/settings_screen.rs` draws the 214px category
      rail (per-group modified counts, `/` filter, `Unknown keys` with
      Levenshtein did-you-mean suggestions from `Resolved::unknown_keys`),
      the per-category content column with the §11 responsive wrap, the
      modified dot as the reset button (`remove_value` → reload; the file
      stays the single source of truth), and the footer (modified-count
      sentence, config path, `Edit as TOML` via `platform::open_path`). The
      widget vocabulary landed with it: segmented selects (≤3 variants),
      the 288px dropdown with doc comments (`window.backdrop`), the − / ＋
      stepper with units, click-to-seek sliders, font rows with ×,
      drag-to-reorder and an append picker (fallback-tagged past the first
      resolvable face), tag chips (a leading `-` kept verbatim), and env
      key/value cells where an empty value renders `unset` and unsets. Every
      edit still goes write-value → reload through the cascade; the old
      modal (`settings_overlay`, its scrim, its exclusive-overlay slot) is
      deleted. Path's `Browse…` is deliberately absent: the app has no file
      dialog yet, and §11 says skip rather than build one for this item.
- [x] Cross-host profile launch (README §12's launch semantics) →
      [#175](https://github.com/zesterm/zesterm/issues/175): `launch_profile`
      resolves `ProfileMeta.host` against the fleet snapshot into a per-tab
      route (`launch.rs` is the pure seam: local/unset stays on the window's
      route, a remote label dials its advertised address, an unknown label is
      a launch that will fail, never a panic). Remote launches push the tab
      **immediately** in a connecting state — placeholder address, the
      chrome's connecting treatment, a provenance line in the pane ("New
      session · profile on host · command", scheme-dim) — and a worker dials
      with three bounded-backoff tries, settling the tab live on attach or
      into the dead-tab treatment carrying the error (the old path was one
      silent `warn!`). `ask_host` preloads the fleet picker with the pending
      launch; its host row launches the profile there. `starting_directory`
      rides `CommandSpec.cwd` locally and the *existing* `CreateSession.cwd`
      field over the wire (no frame growth was needed). Launch-command
      precedence is pinned by test: profile > Defaults > "" remote / resolved
      local shell. The launcher rows regained their host chip, which now
      tells the truth.
- [x] The profiles editor (README §12), replacing the Profiles tab's
      placeholder pane → [#176](https://github.com/zesterm/zesterm/issues/176):
      `chrome/profiles_screen.rs` draws the 248px rail (Defaults pinned
      first, a row per profile with glyph tile / `command · host` sub-line /
      digit, dashed `＋ New profile`; NO discovery line — #145), the editor
      header (34px tile, host chip, Duplicate / Delete — Defaults has no
      Delete; rename rides Duplicate+Delete), the §12 live preview (the
      chip in the window's chrome, only the body in the profile's scheme,
      the caption saying so verbatim), and Launch / Appearance / Cursor
      sections in the settings tab's row shape. `profiles_ui.rs` builds
      rows, inheritance chips and actions in one pass from
      `resolve_profile`: chips track `ProfileProvenance` exactly, the
      launch trio (command / host / starting_directory) never chips, and
      the modified dot appears only on an override — clicking it is
      `remove_profile_value` → reload, never the root file. New §12 widgets
      ride the shared `draw_control` vocabulary: the scheme swatch picker
      (each builtin's normal ANSI row via `zest_theme::resolve`, never
      re-typed), six accent swatches (dimmed AND inert when `color_from`
      says the host decides), six atlas-safe icon tiles, and the host pill.
      Every edit is `write_profile_value` → reload; open tabs restyle live
      through the #162 re-resolve. Keyboard is the settings discipline
      (↑↓ / ←→ / ⏎ / filter / Esc layering ending at close-the-tab), plus a
      *leading* digit 1–9 jumping the rail — once a filter is live, digits
      filter. Deliberately deferred: the fleet-picker-as-host-chooser (a
      picker row launches today; choosing belongs to the cross-host item)
      and `background_image` (#144).
- [x] Per-session palette machinery (README §12's chrome-vs-grid rule) →
      [#162](https://github.com/zesterm/zesterm/issues/162): a tab carries the
      resolved identity of the profile it launched from; its grid keeps its
      own scheme, selection wash and opacity across window theme changes and
      config reloads, and the chip's 2px rule + glyph tile resolve
      profile-vs-host accent per `color_from`. Launch semantics — what sets
      the identity — is the next §12 item.

- [x] **The fleet has no face on the desktop.** → [#23](https://github.com/zesterm/zesterm/issues/23) — **closed**; the sequence below is its record.
      The phone and the web client are both planned to list sessions and attach
      to a chosen one; the app most people will use can only take a `--attach
      <host:port>` on the command line and then guess. The tab strip below is
      *window chrome* and answers none of it: what a tab is when sessions live on
      four machines, how loudly a remote one announces which machine it is, what
      a tab does when its host sleeps, and what opening a window should do at
      all. That last question already produced a bug — opening zesterm adopted a
      shell another machine was driving, because a default was standing in for a
      feature that does not exist.
      **Design settled, implementation under way**: tabs are the sessions this
      window has attached, keyed `(HostId, SessionId)` from the first commit; a picker
      overlay lists every host and session with presence; ⌘T creates on the
      current tab's host; closing a local tab kills, a remote one detaches;
      launch restores the previous tab set, which retires the adopt guess. Both
      orientations — top strip and left sidebar — behind `tabs.position`.
- [x] **Launch restores; the adopt guess is dead.** The tab set persists to
      `state_dir()/tabs.json` (atomic, versioned, state-not-settings — the
      config watcher must not fire on it) on every mutation, and launching
      reattaches exactly those sessions: the active local one synchronously
      in the same startup slot an attach always used (probe unchanged), the
      rest via background workers that cannot let one sleeping host serialize
      the others. A remembered session that ended means a fresh shell, said
      out loud. The GUI never adopts anymore — the answer to "what should
      open do" #23 deferred until the picker existed — while `--attach`
      keeps adopting and `--new-session`/`tabs.restore=false` opt out.
      `--attach-probe` now kills its probe session: with nothing adopting,
      a leaked shell per run would rebuild the pile #23 started from.
- [x] **The picker exists — the fleet has a face.** ⌘K (or +) opens a modal
      overlay over the grid: hosts with presence in words ("andy-mac —
      online", warn-coloured "unreachable"), their sessions with title, cwd
      and attached/this-window tags, and "new session on <host>" rows.
      Type-to-filter, arrows/Enter, click, Esc/scrim to dismiss; the scrim
      swallows every stray click while open (a layout test sweeps the window
      to prove nothing escapes). Rows and their actions are built in one
      pass, so a drawn row and its meaning cannot drift. Attach/create run on
      a worker — a dead host costs a connect timeout, and the picker never
      charges that to the event loop; the finished tab arrives by wakeup.
      Remote dials use the persisted identity (keychain touched on first
      remote need, never at startup) and pin `expect_host` to the roster's
      claim.
- [x] **The app has a fleet model.** `FleetModel` aggregates what the picker
      draws from: mDNS browse (started after first paint — none of it is
      needed to show a prompt), the app-owned dial prober that feeds
      `report_dial` every 10s so `Presence::Unreachable` can actually appear
      (#22's impolite half — the roster is socket-free by design and someone
      must own the dialing), and a watching connection to the window's daemon
      holding its session list fresh through `Hello.watch_sessions` pushes.
      The window's own daemon is synthesized into the listing from its signed
      Welcome, because a default daemon is mDNS-invisible. Everything posts
      one coalesced `Wakeup::FleetChanged` per burst; 0%-idle survives a
      chatty network.
- [x] **Tabs are plural.** `TabStrip` replaces the app's single session: each
      tab is `(SessionAddr, session, local, dead, sized)`, closing is
      policy-aware — local kills, remote detaches, the last one closes the
      window — and ⌘T creates on the current tab's host (one route per window
      until the fleet model; in-process fallback spawns another pty). ⌘W,
      ⌘1–9, ⌘⇧[/], Ctrl+Tab. Background tabs resize lazily on activation so
      a window drag costs one pty message, not N per frame; `Wakeup::Exited`
      is translated per-tab into `TabExited(addr)` so one shell ending closes
      one tab. A `SessionGone` tab stays put marked "· ended" instead of
      vanishing or respawning.
- [x] **Three additive wire fields, no version bump** (CONTRACTS.md has the
      full reasoning): `Keyframe.title` — a complete state finally includes
      the title, so a tab attaching to a running `vim` is labeled at once
      instead of on the next retitle; `Sessions.created` — the daemon names
      the session a create produced, retiring the `.last()` race between two
      concurrent creators; `Hello.watch_sessions` — opt-in listing pushes,
      driven by a registry generation counter bumped on create/close/collect/
      attach/detach and coalesced per connection. Opt-in because an old client
      would mistake an unsolicited `Sessions` for the reply to its own next
      request; a watcher that hears nothing is talking to an older daemon and
      polls. Bindings, fixtures and the web decoder updated in the same
      commit.
- [x] **The daemon conversation is a reusable object.** `DaemonClient`
      (extracted from `remote.rs`'s private handshake): connect/auth once,
      then `list`, `create`, `attach`, `close` — the picker's verbs, no longer
      reachable only through attaching. It captures the host's `HostId` from
      the signed Welcome, which is how the app learns its local daemon's
      identity with zero wire change. `RemoteSession` gains `attach_existing`
      / `create_and_attach` (both `Rebind::Pinned`: a host that answers
      without the session posts `Wakeup::SessionGone` instead of silently
      swapping in a fresh shell), `addr()`, and `kill()` — CloseSession whose
      delivery is guaranteed by consuming self ahead of Drop's writer join.
      Fixed en route: after an adopt-or-create rebind, input kept addressing
      the *old* session — output flowed while keystrokes went nowhere; the
      address is now shared with the supervisor.
- [x] **Chrome text foundations.** `ui_text::emit_ui_run`/`measure_ui_run` in
      `zest-render-wgpu`: shaped with kept advances (issue #5's rule), truncated
      with an ellipsis at cluster boundaries, falling back per character where
      the primary face has no glyph — a CJK cwd or an emoji in a title would
      otherwise be notdef boxes, because `shape_run` shapes only the primary
      face. Glyphs gain `FIXED` (scroll-exempt): `grid_origin` is a global
      uniform, so without a per-instance bit tab titles would ride smooth
      scrolling the day it ships. Shader constant held in sync by a test, like
      `LAYER_SIZE`.
- [x] **Every chord is reachable on Windows.** `Mods::Desktop` was Super-only
      on every platform, and the Windows shell reserves Win+T, Win+W, Win+K,
      Win+P, Win+, and Win+1–9 — so new tab, close tab, the fleet picker, the
      palette, settings and the tab digits could not be pressed at all here,
      while the title bar cheerfully advertised them as `Super+K`. The family
      now takes **⌘ or Ctrl+Shift, both everywhere**, which is the policy
      `is_clipboard_chord` had already settled on, and `chord_label` prints
      whichever the platform can deliver.
      Three things this could not be done naively. `belongs_to_desktop` stayed
      super-only: it is the *pty encoder's* gate, and widening it would have
      stopped Ctrl+Shift+Arrow, the Ctrl+Shift F-keys and vim's `CTRL-^` from
      ever reaching the shell. Matching had to learn which spelling was used,
      because Ctrl+Shift spends Shift on the modifier — `⌘⇧T` must stay
      reserved while `Ctrl+Shift+T` folds onto the `⌘T` row. And the digits
      moved to **positional** matching (`ChordKey::Code`), because Shift+1 is
      `!` on US and `!"#¤%&/()` on the Swedish layout this was built on; that
      fixes ⌘1 on a French Mac on the way past. `@ ^ _` are guarded so
      Ctrl+Shift+6 stays vim's `CTRL-^` on the layouts where it is one.
- [x] **Borderless window, GPU-drawn titlebar and caption buttons.** The window
      wore *two* titlebars on Windows until now — the OS caption above our own
      tab strip — because only the macOS branch hid one.
      **Most of this bullet was wrong about the mechanism, and it is worth
      correcting rather than deleting.** It called for a hand-rolled
      `WM_NCCALCSIZE` and warned that a maximized window needs `top` inset by
      `SM_CYSIZEFRAME + SM_CXPADDEDBORDER` or the tab bar hangs off the
      monitor. winit 0.30 already does both jobs and does the second one
      differently: its own `WM_NCCALCSIZE` handler clamps the maximized client
      rect to the monitor's `rcWork` via `MonitorFromRect`, so no inset exists
      to get wrong. Measured here: maximized client is exactly 1920×1032 on a
      1920×1080 screen, taskbar intact. `with_decorations(false)` +
      `with_undecorated_shadow(true)` is the whole of it, and
      `with_system_backdrop`'s existence means Mica needs no unsafe code
      either — though `window.backdrop` is written against
      `DwmSetWindowAttribute` anyway, because winit discards the `HRESULT` and
      a setting that does nothing on Windows 10 while saying nothing is what
      ADR-003 forbids.
      **What borderless actually costs is the resize edges.** winit has no
      `WM_NCHITTEST` handler, so with the frame gone `DefWindowProc` never
      answers `HTLEFT`/`HTTOP` and the edges silently stop working — while
      maximize and snap keep going, which is what makes it easy to ship
      broken. They come back out of the chrome's own layout pass as
      `HitRegion::Resize`, pushed last so the window edge outranks even a modal
      scrim, and turned into `Window::drag_resize_window`. The caption buttons
      come out of that same pass, glyphs drawn from primitives rather than
      `Segoe MDL2 Assets` — those are Private Use Area, which script-based
      fallback structurally cannot reach.
      Behind `window.custom_chrome`, which became a tri-state (`auto`/`on`/
      `off`) rather than a bool: `schemars` derives the schema default from
      `Window::default`, so a `cfg!(windows)` default would make the *schema*
      platform-dependent and fail `check-schema` on two of three CI legs.
      `auto` is one value everywhere and resolves per platform where it is
      read.
      Verified at the machine: one titlebar, close/maximize/minimize hover and
      act, maximize swaps to the restore glyph, dragging the right edge moved
      it 960→1080, and `--startup-probe` reports 26–30ms against a 100ms
      budget.
- [ ] Snap Layouts: `HTMAXBUTTON` over the maximize rect is what enables the
      Win11 hover flyout, and it needs a real window-proc subclass —
      `WM_NCHITTEST` is *sent*, not posted, so winit's `with_msg_hook` (which
      hooks the message queue) cannot see it. It also suppresses ordinary mouse
      messages over that rect, so hover has to come from `WM_NCMOUSEMOVE`.
      Deliberately separate: the chrome above is usable without it.
- [x] `ChromeHitMap` produced by the layout pass and consumed by **both** the
      renderer and the input path, so visuals and hit regions cannot drift.
      Landed with #23's chrome: `chrome::layout` is the pure pass issue #5
      sketched, and the tests pin the property (tab centres answer as their
      tab, close outranks its tab, a modal picker lets nothing through).
- [x] **Every chord has a name and one table.** `zest-app/src/keymap.rs`:
      `Action` enum + ordered `BINDINGS` (chord policy → action, first match
      wins, table order = the old cascade's precedence), consulted by the
      keyboard dispatch through an exhaustive `App::perform` — a new `Action`
      without a handler is a compile error, and a chord that is not a table
      row cannot exist. Modal overlays (the picker's type-to-filter) stay
      outside the table on purpose: their keys are a line editor, not
      commands. Behavior-preserving; the tricky arrivals are pinned by test
      (⌘⇧[ arrives as `{`, Ctrl+Shift+C arrives uppercase, Shift+PgUp falls
      through to the encoder in the alt screen). This is the rail for the
      command palette (below) and, one day, user-configurable keybindings — a
      config section would layer over `BINDINGS` as data, not a rewrite.
- [x] **The command palette (⌘P; also ⌘⇧P, ⌘/, ⌘? and Ctrl+Shift+/).** ⌘P is
      canonical because it is what fingers actually try, and the desktop
      modifier never reaches the shell, so the chord was dead anyway. Began life
      as a display-only shortcuts sheet and was refactored the same day: a
      list that can *name* every command should also *run* it. Rows come from
      `keymap::palette` over `BINDINGS` — name + platform-spelled chord chip
      (`chord_label`: ⌘ on macOS, Ctrl+Shift/Super elsewhere, keycaps shown
      physically — `⇧[`, not `{`) — and Enter or a click performs the row's
      action through the same `App::perform` the chord dispatches to, so
      "what it says" and "what it does" are one fact, pinned by a
      parallel-list alignment test. Each ⌘1–⌘8 digit is its own searchable
      row. Type-to-filter, arrows skip headers and reference rows
      (mouse gestures from `MOUSE_SHORTCUTS` — no `Key` to replay — and the
      both-conventions footnote), ensure-visible scrolling without wheel
      snap-back, modal by the same window-sweep the picker answers to, and
      "nothing matches" instead of a blank panel. A command with no chord
      becomes representable the day one exists — `keymap::palette`, not
      `BINDINGS`, is the palette's contract.
- [x] **Chrome actually draws above the grid now.** The renderer's doc always
      promised `grid glyphs → chrome rects → chrome text`, but the
      implementation drew each pipeline once over its whole buffer — so
      every grid glyph painted *after* the chrome's panels, and any overlay
      floating over text showed the shell's prompt through its panel (the
      picker shipped with this; a busy screen behind the palette made it
      unmissable). `Scene` now records where chrome begins in the shared
      buffers and the pass draws split instance ranges in the documented
      order; a test pins the boundary bookkeeping, and clearing a scene
      resets it so no frame inherits the last one's split.
- [x] **The schema walk a settings UI renders from.** `zest_config::ui` turns
      the JSON Schema into `UiField`s — dotted key, `x_zest_group`, parsed
      `x_zest_widget`, doc-comment description, min/max range, int-vs-float,
      enum variants (from *both* shapes schemars emits: `oneOf`+`const` with
      per-variant docs, and plain `enum` arrays — today's schema contains
      both), schema default, `x_zest_restart`. Outside the `fs` feature so
      the web client can reuse it. `UI_EXCLUDED` names the two keys skipped
      on purpose (`schema_version`, `profiles`) and a test holds the walk to
      exactly-once coverage of every other schema key. `toml_edit` is
      re-exported (fs-gated): `write_value`'s signature names its `Value`, so
      callers get the type from the same place as the function.
- [x] **The settings overlay (⌘,), read-only browse.** The third modal on the
      picker recipe, rows *generated* from `zest_config::ui::fields()` by the
      pure `settings_ui::build_rows` — schema coverage is a test, so a new
      setting appears without a UI change. Two-line rows: humanized label +
      value cell (toggle/select/slider/number/text; list-shaped values
      read-only for now), then dotted key + doc-comment summary with tags on
      the right. Tags tell the truth: "set by profile `k8s`" chips from the
      kept cascade provenance (warn-coloured when the source outranks the
      user file, because an edit there will be visibly shadowed), "applies on
      next launch" from `invalidate::class_of` (the authoritative table, not
      the sparser `x_zest_restart`), and "not applied yet" from
      `NOT_YET_WIRED` — settings the schema declares but the app does not
      consume; deleting the entry is part of wiring one. Type-to-filter
      (Esc layers: clear filter, then close), arrows skip headers,
      keyboard navigation ensure-visible-scrolls without the wheel snapping
      back (tested), ⌘K/⌘//⌘, switch between the three overlays.
- [x] **Settings edit inline and apply instantly.** Enter/→ flips a toggle,
      cycles a select, live-previews the next theme; ←/→ step numbers to the
      *next grid point* in the travel direction (so repeated presses cannot
      accumulate float noise — every result is a value the user could have
      typed); Enter on a number opens a typed buffer (chars go to the buffer,
      never the filter — the mode collision is resolved by construction),
      parse-clamped on commit, error-coloured on garbage. Every change goes
      one way: `settings_ui::to_toml` → `zest_config::write_value` into the
      user's `config.toml` (comments preserved; first edit creates the file)
      → a **synchronous** `reload_config` so a toggle feels like a switch —
      the watcher's 120ms echo then diffs to `Invalidation::None` and no-ops.
      The file stays the single source of truth; the overlay never holds a
      value the file does not. f32→f64 widening noise is scrubbed before it
      can reach the file (`clean_float`, pinned by test against the schema's
      own noisy `spring_response` default). Restart-class edits join a
      banner pinned above the list — the user-visible surface the old
      `tracing::warn!` never had — and a failed write banners too, instead
      of pretending. Verified live: cycling the theme wrote
      `[appearance] theme = "nord"`, re-themed the window instantly, and the
      row's chip flipped to "set by config file" through the real cascade.
- [x] **Settings polish: strings, sliders, honesty for lists.** Text/path
      fields edit through the same typed buffer (seeded with the current
      value); sliders click- and drag-to-set against the *track the layout
      actually drew* (`ChromeLayout.settings_tracks`, tested), quantized to
      the arrow keys' twentieth-of-travel grid so a drag and a keypress
      agree about which values exist — and applied only when the quantized
      value changes, so a full drag is at most twenty writes. List-shaped
      rows say "edit in config.toml" in their description instead of letting
      Enter silently do nothing. Found live and fixed: **winit delivers the
      spacebar as `Named(Space)`**, so no overlay filter or edit buffer
      could ever contain a space — picker included, since it shipped; an
      empty filter result now says "nothing matches" instead of presenting
      a blank panel as if broken. Drive-by: `watch_config` now watches
      `zesterm.toml` in portable mode instead of a `config.toml` nobody
      writes.
- [x] **The typed profiles layer** (design screen 12's config half,
      [docs/design/client-ui/](design/client-ui/README.md) §12; #130).
      `zest_config::profiles` — outside the `fs` feature, like `ui`, so the
      web editor renders from the same resolution. `ProfileMeta` parses the
      profile-only keys (`command`, `host`, `tab_title`, `color_scheme`,
      `tab_color`, `icon`, `color_from`, …) leniently — a wrong type warns
      and falls back, never fails — and `profile_layer` now *strips* them, so
      launcher/chrome inputs stop spraying `unknown_keys` on every
      profile-tab launch. `profiles.defaults` is a reserved parent layer:
      `load()` inserts it beneath the named profile (user <
      profiles.defaults < profiles.<name> < workspace), it is hidden from
      `list_profiles`, and `resolve_profile` reports per-key
      overrides/inherited/unset — the editor's chips — through the cascade's
      own merge, so `shell.env` replaces wholesale here too.
      `PROFILE_SETTINGS_KEYS` scopes the editor's rows (the cascade still
      takes any key — the k8s-prod red window stands), `profiles::fields()`
      hands the editor renderable rows (four new `Widget`s: host-, scheme-,
      accent-, icon-picker), and the fs side gains
      `write_profile_value`/`remove_value`/`remove_profile_value`/
      `remove_profile`/`copy_profile` — a dotted profile name stays one key,
      removals prune emptied tables, comments survive. A profile edit is now
      `Invalidation::Free`: the launcher re-resolves live.
- [x] **Screen 1 of the design handoff: the title bar, tab chips and status
      bar** ([docs/design/client-ui/](design/client-ui/README.md)). The chrome
      speaks the design's type scale now — `Fonts::set_ui_px` threads a per-run
      size through shaping/keys/advances (cell geometry untouched, grid path
      leak-proof by test) — and obsidian authors its full sigx record, with the
      three off-token surfaces (`titlebar_fill`, `block_header_fill`,
      `soft_hairline`) derived in `zest_theme::derived` as OKLCH steps so light
      themes step in their own direction. The 46px bar carries spec chips
      (196–240px, two lines, host-accent dot, top accent rule, fill meeting the
      pane) and the two pills; ⌘⇧E / the pill flips `tabs.position` through the
      settings write path. The 28px status bar says cwd · ⎇ branch (a HEAD
      read, never a subprocess) · blocks | theme · link path · latency — the
      fleet prober now keeps the RTT it was already paying for, and
      `FleetHost` carries `Reachability` instead of discarding it.
- [x] **Screen 2: the sidebar is the fleet-scale layout.** Host-grouped
      session rows (uppercase tracked group labels — `ui_text` grew per-cluster
      tracking for exactly this — with accent dots and path/latency sub-labels
      from the fleet), a search affordance that opens the picker, state dots
      (running/live/idle), an age column stamped by the wake callbacks
      (`ActivityMap` — the one place every session kind already reports output
      through), the 42px fleet footer, and the slim 44px title bar with the
      session name, cwd, host chip and the way back to horizontal.
- [x] **Screen 6: ⌘K becomes the fleet's search.** The picker grew into the
      design's palette: one 620px panel, 88px down, with a ❯-query row
      ("N hosts searched" on the right), grouped results — **Blocks first**,
      the history of what ran anywhere, gathered from every attached tab with
      provenance (`host · 2m ago · exit 0`) from the block timestamps; then
      Sessions, Hosts (Enter opens a shell there), and Actions from the
      keymap through the same `perform` their chords use — and the keycap
      footer. ⏎ runs a block here; ⇧⏎ runs it in the session it came from,
      the honest half of "run on host…" until a chooser exists. Selection
      skips group labels, ensure-visible scrolling without wheel snap-back,
      "nothing matches" instead of a blank panel, and the modal sweep test
      now also proves group labels are *not* clickable.
- [x] **Screens 7 and 8: the fleet directory and the theme gallery.** Full-pane
      screens over the grid (Esc returns; chords still work; bare typing never
      falls through to a shell nobody is looking at), sharing one page frame.
      Fleet cards say only what is known — path with measured latency, key
      fingerprint, session count; asleep hosts go dashed (a layout-side dashed
      border, the SDF stroke cannot dash) with no wake button offered because
      wake-on-LAN does not exist yet. Theme cards render their preview in each
      theme's *own* bg/fg with the green/blue/red applied, the swatch strip is
      builtin.rs's normal ANSI row read not re-typed, and clicking a card
      writes `appearance.theme` through the settings path — instant re-theme
      via the same synchronous reload the overlay uses. Entered from the
      sidebar's fleet footer and from ⌘K's Actions group (chordless rows, the
      palette contract's first use of one).
- [x] **Screen 5: split panes.** ⌘D gives the active tab a second pane on the
      same host (and thereafter moves the keyboard between panes); the
      renderer's day-one `&[Viewport]` slice finally gets its second element.
      The whole refactor hinged on one function: `TabStrip::active_source`
      now returns the *focused pane's* source, so all twenty input, selection,
      IME, status and block call sites reroute without being touched.
      `focused_view_rect` is the one pixel↔cell truth (cell_at, block headers
      and fold maps all read it); pane frames and headers are drawn by the
      chrome from the same `pane_frames` math the viewports use, so the
      border cannot miss the grid it frames. Clicking the unfocused pane —
      anywhere — moves focus; the focused body stays the terminal's (tested).
      ⌘W closes the focused pane first (closing the left promotes the right,
      so the tab keeps its identity); a pane's shell ending collapses the
      pane, never the tab. Splits deliberately do not persist yet.
- [x] **The design's four animations, on one shared clock.** Cursor blink
      (finally consuming `cursor.blink`/`blink_interval_ms` — removed from
      `NOT_YET_WIRED`), the palette caret on the same phase, the running
      ring's orbiting gap (an SDF box cannot draw an arc; a fill-coloured
      bite orbiting the ring reads the same), and the sidebar dot's 1.6s
      pulse. Phases derive from one epoch — never stored, so a missed tick
      cannot desynchronize — and `about_to_wait` schedules exactly one
      `WaitUntil` when something on screen animates, `Wait` otherwise: a
      resting window schedules nothing, which is the settle guarantee in one
      place. Degraded states landed with it: a dropped daemon link turns the
      status segment "reconnecting" in danger until `Reattached`, and a block
      still "running" when its host went away shows a faint rail and says
      "interrupted".
- [ ] Animation clock, the *spring* half. Springs `(response, damping)`, not
      easing curves — terminal motion is interruption-dominated and a spring
      absorbs a changed target with continuous velocity for free. Substep the
      integrator (`h = dt/ceil(dt·240)`) or a spring tuned at 60Hz behaves
      differently at 144Hz. The periodic clock above is the rail it plugs
      into; springs arrive with tab/window motion.
- [ ] Smooth scroll as a fractional row offset, **suppressed in the alt screen**.
- [ ] `reduce_motion`, honouring `SPI_GETCLIENTAREAANIMATION`.
- [ ] Per-OS backdrop: Mica via `DWMWA_SYSTEMBACKDROP_TYPE`.
- [ ] Polish: OSC 0/2 title, DECSCUSR cursor styles, font zoom, DPI changes.
- [x] Box drawing and block elements are generated at cell size (`zest-font`'s
      `boxdraw`), not taken from the font. A font's glyph is as wide as the
      font's advance and the cell is that advance *rounded*, so a run of `█`
      rendered as a picket fence and every table border had seams — measured at
      8×17 in an 8×18 cell. `typography.builtin_box_drawing` turns it off for
      anyone who wants a particular font's own. Found on Windows (#81), but the
      arithmetic was never platform-specific.
- [x] Stem darkening applies to **glyph coverage**, and the settings that
      control it reach the renderer at all (#82). It ran on the whole
      framebuffer before, so a theme's `#0D0D0D` background resolved to `0x20` —
      a text setting was quietly repainting every background in the window, and
      `appearance.text_gamma` could not turn it off because `Renderer::tuning`
      was assigned `TextTuning::default()` once and never touched again. One
      default now, asserted equal across the two crates that hold it.
- [x] Text is sampled **per subpixel**, and outlines are no longer grid-fitted
      (#100, #84). swash pins the hinting target to horizontal LCD
      (`hinting_cache.rs`) and exposes only `hint(bool)`, so every glyph was
      grid-fit for three times the horizontal resolution and then sampled once
      per pixel. That changes shapes, not sharpness: `w` at 13 ppem in Cascadia
      Mono came back as three vertical stems and read as `W`, and `o c e C t`
      lost the baseline overshoot `a` kept, so "Close" read a pixel short beside
      "tab". No gentler hinting target exists — skrifa driven directly with
      `Target::Smooth { mode: Light }` returns a byte-identical bitmap, because
      the face is ClearType-aware. The two symptoms have different axes, so the
      fix is both halves: per-channel coverage for the horizontal one, no
      hinting for the vertical one. `appearance.text_antialias` puts it back.
      Blended with dual-source `OneMinusSrc1`; grayscale where the adapter or a
      translucent window says no. ADR-010.
- [x] Text reads right beside Windows Terminal (#111), which took four goes and
      is worth reading ADR-011 for. Coverage was being applied in *linear* space
      and sRGB-encoded after, so 20% coverage emerged at 48% brightness — every
      edge inflated, every counter filled, and the whole window read fat. It is
      linearized now. `size_pt` also meant *pixels* despite its name, so the
      default was ~9.75pt against every peer's 11-12; it means points. Defaults
      are grayscale coverage, grid-fitted, stem darkening 2.5 — which is what
      Windows Terminal itself does, measured: its channel spread on inked pixels
      is 0.0, so it is not doing subpixel at all. `text_antialias` and
      `text_hinting` are separate settings because the mixed pair is the one
      that matters, and the chrome is pinned to the good configuration rather
      than following them. Also: cell width stated the way Windows Terminal
      states it, and a searchable font picker, because cycling 266 families with
      an arrow key is not a picker.
- [x] Validated that default side-by-side against Windows Terminal (#111). It
      was 1.3, which was compensating for the linear-coverage bug and pushing
      the wrong way; it is 2.5, tested against a light background and a dark one
      and preferred on both. The warning that stood here — that it "ships broken
      constantly and reads as looks slightly off" — was right, and it took a
      user saying the text looked fat to act on it.
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
- [x] **Kitty keyboard protocol (CSI u), flags 1, 2 and 8.** Disambiguate
      escape codes, report event types, report all keys as escape codes. The
      terminal owns a per-screen flag stack — a crashed full-screen program
      cannot leave the shell encoding keys its way — and the flags reach a
      *remote* client as three new `Modes` bits, so encoding still happens at
      the keyboard with no protocol change and no version bump.

      Three things worth knowing before touching it. **`CSI u` with no
      intermediate is still SCORC**, and the arm that used to match it with any
      intermediate was executing every kitty sequence as a cursor restore.
      **F1–F4 use the `~` form**, because `CSI 1;m R` is CPR. And the flags
      only reach an attached client because `sync_kitty_modes` bumps `seq`;
      without that the local window looks perfect and every remote session
      encodes the legacy way at a program that has stopped expecting it.
      **Ported to the web client in the same commit**, because the two encoders
      serve one session and this is the case where drift is not cosmetic: a
      program that turned the flags on has stopped expecting the legacy form, so
      a browser tab that kept sending it types into a void while the native
      window works. `clients/web/packages/input/src/kitty.ts` mirrors the Rust
      case for case, its tests assert the same bytes, and the app now binds
      `keyup` — which the DOM delivers for everything, where winit's releases
      were filtered before the encoder ever saw them.
- [ ] Kitty flags 4 (alternate keys) and 16 (associated text). 4 needs the
      base-layout key, which winit exposes through a trait that does not cover
      Wayland — a platform-capability question, not a table to fill in. 16 is
      what would let an IME commit reach a program running under flag 8.
- [ ] Keypad keys as separate keys under flag 1 (`CSI 57399…57427 u`). Left out
      of the first pass rather than guessed: the numbers want checking against
      `kitty +kitten show_key -m kitty`, and wrong key numbers are worse than
      absent ones.
- [ ] `Ctrl+Tab` is swallowed by the binding table before the encoder sees it
      (`keymap.rs`, `When::Always`), so it cannot reach a program as `CSI 9;5u`
      — which is exactly what Helix and neovim configs bind now that kitty made
      it expressible. The fix is a third `When` variant, not an if-block.
- [ ] `CSI > c` (DA2) and `CSI = c` (DA3) are answered with DA1, from the same
      wildcard-intermediate mistake as the `u` arm ten lines away. Harmless
      today and on the kitty probe path, so worth fixing deliberately rather
      than as a drive-by.

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
- [x] **Do not go borderless on macOS.** It costs traffic lights, native
      full-screen, Sequoia tiling and accessibility, and gains nothing over
      `titlebar_transparent` + `title_hidden` + `fullsize_content_view`. The
      traffic-light inset is not a constant — recompute on full-screen changes.
      **Landed with WS-A's chrome, exactly as planned**: the attributes are
      flags (startup probe unchanged at 46ms), and `platform::
      traffic_light_inset` asks AppKit for the cluster's extent per chrome
      layout — `None` in fullscreen, where the buttons auto-hide. The
      horizontal strip reserves that width as a drag zone; the sidebar gives
      the cluster a drag header band and runs the grid full-height beside it.
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
`perform.rs` edits, which WS-F also reads. Block headers, rails, fold affordance
and the palette's block rows are specified in
[docs/design/client-ui/](design/client-ui/README.md) (screens 3 and 6).

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
- [x] **Shell integration for PowerShell** (#83). Both PowerShell 7 and Windows
      PowerShell 5.1, which is why nothing newer than 5.1 syntax appears in the
      hook. PowerShell has no `ZDOTDIR` analogue and `$PROFILE` is a file
      belonging to the user, so the injection point is the command line itself —
      `-NoExit -Command ". <shim>"`. `install` therefore returns an `Injection`
      of environment *and* command-line halves rather than a list of variables.

      Three things had to be true and were not:

      - `Shell::detect` split the command line on whitespace, so Windows' own
        default shell — quoted, because `C:\Program Files` has a space in it —
        had the executable `C:\Program` and matched nothing. **Every** Windows
        shell was unhooked, with a status bar reading `0 blocks` as the only
        sign; the "no shell integration for this shell" log was at `debug`, and
        is now at `info`.
      - The app forwarded its *built* command line to the daemon, which would
        have detected a PowerShell in it and injected a second time — doubling
        every marker, which the parser reads as an empty block between each real
        one, and quietly overriding `--no-shell-integration`. It now sends what
        the user configured, empty meaning the host's own default, exactly as
        `new_tab` and `split_right` already did.
      - `633;P;Cwd=` was read literally though VS Code's dialect escapes it, so
        anyone using VS Code's own PowerShell integration — the zero-code
        workaround this issue pointed at — had a cwd of `C:\x5cDev\x5czesterm`
        in the status bar.

      The hook chains the user's `prompt` rather than replacing it, states the
      command with `633;E` instead of leaving zesterm to read it back off the
      grid — PSReadLine repaints the line as it predicts, so the cells are a
      rendering of the command and not the command — and reports the working
      directory *before* `133;A`, since that marker opens the block and stamps
      the cwd known at that moment onto it.

      Verified against a real interactive `pwsh` 7.6.4 under ConPTY:
      `echo hello` → `D;0`, `cmd /c exit 3` → `D;3` — the command's own exit
      code, which `$?` alone could not have produced. The recording is in the
      corpus as `blocks-pwsh.vtrec`. `zesterm --shell-integration pwsh` prints
      the same hook for `$PROFILE`, via
      `| Out-String | Invoke-Expression`.

      `default_shell()` now resolves PowerShell on `PATH` before the one
      hardcoded install location, then Windows PowerShell 5.1, then `%COMSPEC%`.
      A scoop, winget or MSIX pwsh previously fell all the way to `cmd.exe`,
      which has no prompt-function mechanism and therefore no command blocks,
      ever — that is out of scope rather than pending, and is the one shell here
      that gets a permanent no.
- [ ] **bash, fish and WSL.** bash and fish are deliberately not written yet:
      neither can be *seen working* on the machines this is built on. There is no
      fish, and `/bin/bash` on the Mac is 3.2.57 — Apple's patched build, where
      the `ENV` startup path injection depends on is disabled, and which Ghostty
      excludes on Darwin outright for that reason. Writing them blind is how the
      attach path nearly shipped compiled and unseen.

      WSL is the next mechanism rather than the next shell, and it is the case
      `Injection`'s two halves exist for: `WSLENV` is the only way a variable
      crosses into the distro, *and* the inner shell still has to be named on the
      command line. It also needs `Shell::detect` to look past the first token —
      `wsl.exe -d Ubuntu -- bash` is a bash — which is why that is a token walk
      rather than a one-line match.
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
- [x] **Block folding, and the headers to fold from** (design screen 3,
      [docs/design/client-ui/](design/client-ui/README.md)). Headers are a
      per-frame pure pass over the visible rows — state rail, recomposed
      command, cwd/duration/exit metadata from the new block timestamps, hover
      action chips — drawn over the block's prompt rows; the live prompt is
      never overlaid and the alt screen is skipped. Folding is the planned
      renderer change, landed: `Viewport.row_map` names the absolute rows to
      draw (`fold_row_map` compacts over folded output, pulling scrollback
      in), and the row loops, selection, the cursor, preedit and every mouse
      row→line site read that same list — `visual_line_at` is the one
      translation. Fold state is per-session, per-window, never on the wire:
      two clients watching one session may disagree. Folded headers count
      their hidden lines; ranges are inclusive because the parser already
      pulled `D` back onto the last output row (a one-line output folds).

      Three things the first cut got wrong, all found by using it (#124).
      `fold_row_map` padded its blank filler *before* reversing, so the filler
      landed at the top and the surviving rows sank by exactly the number of
      lines hidden — fold the only command in a fresh session, where there is
      no scrollback to pull in, and the header ended up on the last rows of an
      empty screen. The chevron was drawn and its hit region pushed
      unconditionally, while the fold declines a block with no `end_line` or no
      output, so `cd ..` and every running command offered an affordance that
      did nothing; `fold_range` is now the single predicate both read. And
      `ESC[2J` left the index describing rows it had erased — see the `cls`
      note below.

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

- [x] **What `cls` destroys** (#124). `ESC[2J` blanks cells without renumbering,
      so every block kept claiming line ids whose content was gone — and the
      shell reuses those very ids for the next prompt. A stale block *has* an
      `output_line` where a fresh prompt does not, so the header pass drew the
      old command over the row being typed on: opaque, and it ate the click
      too. It reads exactly as "I can't type any more". RIS already cleared the
      index and said why in a comment; ED is the same situation.
      `BlockIndex::erase_screen` keeps only what lies entirely above the erased
      region, and **modes 2 and 3 only** — a line editor emits mode 0 on every
      keystroke, and invalidating there would delete the block being typed
      into. `finish` also had to stop reopening an already-finished block,
      since pwsh emits `133;D` from its prompt function, which runs *after*
      `Clear-Host` deleted the block that `D` was for.

      The wire half is not optional: the window is a client of its own daemon,
      `diff_blocks` cannot express a removal and the applier only upserted, so
      the fix was invisible in the app until `Keyframe.blocks_from` carried it.
      See CONTRACTS — the interesting part is that eviction and destruction had
      to become distinguishable, which one number does by rising for the first
      and falling for the second.

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
- [x] **A shell that exits is noticed on Windows, and takes its children with
      it.** Two gaps with one root: on ConPTY nothing tells the reader the child
      is gone — the output pipe's write end is held until the pseudoconsole
      closes — and `TerminateProcess` reaches one process where unix's `SIGHUP`
      reaches a group.
      `ConPty::watch_exit` waits on a duplicate of the process handle (the OS's
      own wait, never a poll) and then **waits for the output to go quiet**
      before reporting. That second part is what the backed-out attempt was
      missing, and why re-landing it verbatim would still have been wrong:
      `exited` going true is what eventually reaches `ClosePseudoConsole` via
      the registry sweep, so reporting it the instant the wait returns cuts off
      the tail ConPTY had not painted yet (gotcha 2c). The watcher never touches
      the HPCON, so the reader is still draining when the close happens (gotcha
      1). `an_exited_session_is_kept_until_nobody_is_watching` and
      `a_session_is_not_swept_before_anyone_has_attached` are no longer
      `#[cfg(unix)]`, and they are the acceptance criteria.
      A job object with `KILL_ON_JOB_CLOSE`, assigned atomically through the
      same `CreateProcessW` attribute list the pseudoconsole already uses, gives
      `hangup` a process *tree* to end. The test found a bug in the first cut:
      `hangup` returned early when the shell exited politely, so a well-behaved
      shell's detached `ping` survived — `ClosePseudoConsole` signals the
      console's clients and says nothing to anything started detached. The job
      goes regardless now. Nested-job refusal (VS Code's terminal, some CI
      runners) falls back to no job with a warning, because a leaky process tree
      beats a terminal that will not open.
      The app's in-process fallback pty never called `watch_exit` at all, so
      `exit` in a `--no-daemon` tab was doubly dead; it does now, and both
      reporters are gated on one `swap` so one shell cannot close two tabs.
      Verified at the machine: `exit` closed the window in 0.5s, the daemon
      survived it, and no shell was left behind.
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
- [x] **The daemon serves WebSocket** (`--listen-ws`, default port 7718, off by
      default like `--listen-lan`). The transport browsers can actually reach:
      a hand-rolled server-side RFC 6455 codec in `zest-daemon/src/ws.rs` —
      hand-rolled because `serve` requires independently owned read and write
      halves and sync tungstenite cannot be split without a mutex deadlock or
      two unsynchronized writers interleaving a pong into a keyframe; the
      module docs carry the full argument. The WebSocket layer is a byte pipe:
      the identical length-prefixed MessagePack stream, one binary message per
      write batch, whole frames only — so `serve` is untouched and the
      browser's streaming `FrameReader` runs unchanged. Same Ed25519 handshake,
      same `accept_hardened` watchdog/cap/cooldown posture as the LAN,
      deliberately no Origin check (auth is the signature, not ambient
      authority) and deliberately no TLS yet (localhost is a secure context;
      M4's tunnel terminates wss at the edge; LAN ws:// is parity with raw
      TCP). Proven end to end by `tests/ws.rs` with tungstenite as the
      *independent* client, and by `attach --ws`, which is also the
      layer-isolating debug tool for everything the web client will build.
- [ ] SQLite scrollback. Scrollback is in memory and bounded; a session that
      outlives its window does not yet outlive the daemon.

### WS-G — Web client

`clients/web/`, plus the Rust that generates what it is checked against — the
exporter cannot live in TypeScript, and "no path ownership" is the rule now
anyway.

The decoder was built before the daemon could be reached at all, which was the
point: a browser cannot open a unix socket or raw TCP, and for a long time the
daemon spoke nothing else. **That blocker is gone**: ADR-005's binary WebSocket
data plane exists (`zest-daemon --listen-ws`, WS-F above), so everything below
is now unblocked and building.

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
- [x] Grid renderer: `@zesterm/render`, **Canvas 2D**, behind the "given a grid
      and its dirty rows, paint" seam — repaints are row-scoped because deltas
      name their rows, `fillText` inherits the browser's font fallback, colour
      emoji and PUA icons (the `Zyyy` trap, already paid for once), and
      backgrounds coalesce before glyphs so a wide char's spacer cannot erase
      its right half. (**`@sigx/terminal` cannot be reused** — it paints TSX
      *to* a TTY, the inverse job.) Swap in an atlas backend on measurement —
      a large grid repainting most rows below 60fps — not on instinct.
- [x] SignalX app: session list, attach, input — `@zesterm/app`, with the
      control plane on `@sigx/actors` 0.7.0 exactly as ADR-005 draws it: a
      `SessionDirectory` actor hosted by the sidecar (`@zesterm/sidecar`,
      standalone Node in v1; M4's Bun-child-of-daemon shape is a packaging
      change over the same code), fed by a loopback `watch_sessions` client,
      read live over the actors WebSocket. Grid deltas never touch the actors
      socket — the terminal view dials the daemon's binary WebSocket directly,
      at an address it learned *from* the directory. V1 cuts, named: no
      selection/copy, no mouse, no scrollback paging, no splits, no palette;
      device identity is a localStorage seed until M4's enrollment. (IME was a
      cut for one day: the first live run typed an emoji and the shell
      received nothing, so composed-text input landed — a hidden textarea
      whose composition commits ride `encodeComposedText`, un-bracketed
      because a commit is typing, not a paste.)
- [x] **The client stops transcribing the Rust** — `cargo xtask export-web`,
      gated by `check-export-web` and now the seventh gate. It writes the
      settings JSON Schema, `zest_config::ui`'s walked `UiField`s, and the
      built-in themes as a typed TypeScript module. Two claims the tree had
      been making and not keeping: `schema.rs` says settings UIs are
      *generated* from the schema, and `ui.rs` keeps the walk outside the `fs`
      feature so a browser can run it — yet no TypeScript read either, and
      `builtin.ts` carried hand-copied hex whose own doc comment named this
      exporter as the fix. The port is proven faithful the only way that
      counts: the 18 existing theme tests, written against the hand-copied
      values, pass unchanged against the generated ones. The themes are emitted
      as source rather than JSON so `tsc` fails when the Rust grows a field the
      TypeScript `Theme` does not have. New `@zesterm/settings` package
      (zero runtime deps) is where a generated settings form will be built.
- [x] **The client becomes deployable, and learns which world it woke up in** —
      `cloud/`, the directory WS-H's row has claimed since the start and which
      did not exist. A Worker with static assets (not Pages: same-origin is
      what lets Phase 2's `__Host-` cookie cover the app and the API with no
      configuration) serving the built app and `/api/*`. One `vite build` now
      serves loopback *and* the edge, because the app asks `/api/bootstrap` at
      runtime instead of reading a `VITE_*` baked in at build time — the
      sidecar answers `local`, the Worker answers `cloud`.
      A third pnpm workspace with its own lockfile, deliberately: `clients/web`
      has no build tooling but vite and its dependency policy is a feature, so
      `wrangler` stays out of that tree. The two share one thing, the built
      app, and share it as a *path*.
      Hosted, it renders an honest "cannot reach your machines yet" card rather
      than the real session list: there is no sidecar at the edge to host
      `SessionDirectory`, and **mixed content forbids an https page from
      dialling `ws://` on the LAN at all** — so the deployed client will route
      *every* session through the relay, including to the machine in front of
      you. Verified under real `workerd` and in a browser, both paths.
- [x] **An account exists, and the Worker knows who you are** — GitHub OAuth
      hand-rolled on the Worker, D1 for users, identities and sessions.
      `/api/bootstrap` stops saying `user: null`.
      The session cookie is opaque and `__Host-` prefixed; what is *stored* is
      `sha256(token)`, so a dump of `sessions` is a list of hashes rather than
      a set of usable cookies. CSRF is `Origin` **and** `content-type:
      application/json` with no CORS headers anywhere on `/api/*` — a form POST
      cannot set the header and a `fetch` that does triggers a preflight the
      Worker never answers. No tokens, no double-submit.
      Identities key on the provider's **numeric** id: GitHub logins are
      renameable and reusable, so an account keyed on one is inherited by
      whoever claims the name next. Two identities merge into one account only
      when **both** assert a verified address — linking on an unverified one is
      a complete account-takeover primitive, and it has a test that names it.
      Tested against real SQL: `node:sqlite` runs the actual migration, so the
      foreign keys and the `email_verified = 1` filter are exercised rather
      than mocked. The OAuth round trip runs against a stubbed GitHub including
      the failures nobody clicks on purpose — mismatched state, expired state,
      and GitHub's 200-with-an-error-body, which is the one that otherwise
      surfaces as a 401 far from the expired code that caused it.
      Google is prepared for: one file, one registry entry, one secret.
      → the login gate and the account menu are the client half, next.
- [x] **You can sign in, and sign out** — `@sigx/router` with the login gate as
      a `beforeEnter`, a `/login` screen, and an account menu. The gate
      **redirects** rather than returning `false`: on a direct page load `from`
      is `null`, and `false` merely blocks the navigation while leaving the
      protected component on screen, so a gate that returns it is bypassed by
      pasting a URL.
      Nothing gates the **local** path — reaching `127.0.0.1:7350` *is* the
      authority, and the daemon still challenges the device key underneath. One
      `vite build` serves both, so the session list is unchanged there.
      Sign-out is a `POST` with `content-type: application/json`, because that
      is exactly what the CSRF rule requires; a link or a form is refused 403.
      Verified in a browser on both paths, signed out and in.
- [ ] **The device registry** — the account's list of machines and browsers.
      The Worker's half is in: `POST /api/enroll/code` mints a one-shot code
      for the signed-in person to carry, `POST /api/enroll/claim` is answered by
      the daemon with an Ed25519 signature over it, and `GET /api/hosts` /
      `GET /api/devices` plus the two revoke routes are the caller's own or
      nothing.
      **The claim verifies the signature before it spends the code.** Reversed,
      anyone who can reach the endpoint burns codes without holding a key, and
      the person minting them never gets a machine enrolled. Spending is then a
      compare-and-set in one statement — D1 has no transaction across two
      `prepare` calls — so a replayed claim matches no row rather than enrolling
      twice.
      **`/api/enroll/claim` is the one route exempt from the `Origin` half of
      the CSRF rule**, named as such in the router rather than special-cased in
      a handler. A daemon is not a browser and sends no `Origin`; the exemption
      is sound only because the route reads no session cookie, so there is no
      ambient authority to forge. It still requires
      `content-type: application/json`, which keeps a victim's browser off it.
      The preimage is a byte-for-byte port of `zest-mesh`'s, pinned to it by
      `zest-mesh`'s own golden hex **and** a signature the Rust actually
      produced — the two implementations share no code, and a drift otherwise
      surfaces at bring-up as a mismatch that names neither side. Verification
      is `zip215: false`, matching dalek's `verify_strict`: noble's default
      accepts small-order keys, which verify almost anything.
      Still open: the devices screen. The daemon's `--enroll` now posts for
      real — but the two halves have still never spoken over a network; each is
      tested against the shared preimage, not against the other.
- [x] **The fleet screen reads the account** — `GET /api/hosts` and
      `/api/devices`, with revoke on both.
      The plan called for a separate `/settings/devices`; that was wrong and
      the duplication is why. A devices screen listing machines would shadow
      the fleet screen, which already exists. But `hosts` and `devices` are not
      the same set: a browser holds a key that can attach to every machine you
      own and will *never* appear in a fleet listing, because it serves no
      sessions. So machines fold into the fleet view, and browsers and phones
      get the section they had nowhere else to be — which is where a stolen
      laptop's key is revoked.
      **Enrolment is the spine, discovery decorates it.** The account's list is
      durable and survives a machine being asleep; presence attaches once there
      is a relay to learn it from. Until then `last seen` is the only honest
      thing to show. A seed-backed key is named as such on screen — a browser
      on the fallback path is working, not secure, and a row that looks like
      every other row is a comfortable lie about the one that matters.
- [x] **The tabbed chrome** (#150; `docs/design/client-ui/` §1–§2) — the shell
      rewritten around one `TabsState` signal and the pure reducers: the
      horizontal strip (46px title bar, 34px chips, the active one kept in
      view by a tested predicate), the vertical layout's 262px sidebar whose
      host groups derive from the SAME tab list the strip renders (invariant
      6 — a second array cannot show the session just started), and the `+`
      launcher menu, the one way to start a session, `⏎` running the default.
      Two orderings are load-bearing: chords are claimed at window *capture*
      before the terminal's encode path can see them, and navigation is an
      EFFECT of activation — the route watcher behind `/h/:hostId/s/:sessionId`
      only ever activates, one direction, so the URL and the tabs cannot chase
      each other. Everything decidable without a DOM — chip labels, launcher
      rows, the scroll-into-view and menu-alignment predicates, the icon-rail
      breakpoint — lives in `chrome-model.ts` under `node --test`, and the
      mock's fabricated data (latency, ages, profile rows) is omitted rather
      than faked, the spec's own rule. Still ahead here: the palette the ⌘K
      affordances dispatch to, and the blocks pane — each its own item.
- [x] **The primary screen renders as command blocks** (#151) — the design
      README's §3–§4 in DOM. `TerminalView` becomes a mode switch on
      `altScreen`: `BlocksPane` for the shell, the extracted canvas `GridPane`
      for full-screen apps, with the hidden IME textarea, focus handling and
      the connection banner shared so switching screens drops neither focus
      nor a mid-flight composition. Every header decision — rail colour,
      'exit ?' never a green tick, foldable only with output, the interrupted
      predicate, the three duration shapes — lives in a pure `paneModel`
      under `node --test`, proven on the `blocks-zsh` recording as well as
      synthetic states; the component only turns items into elements. ⌘⇧O and
      ⌘⇧R ride one selector (`mostRecentBlockWithOutput`) so copy and re-run
      can never disagree about their target, and re-run types only while the
      trailing block is an open prompt — mid-command, the replay would land
      in that command's stdin. The pane follows output (pinned to the bottom
      until the user scrolls up to read, re-engaging when they return — a DOM
      scroller holds position where the canvas always showed the live
      screen); folds key on the full (host, session) pair. Degraded states
      per §4: reconnecting dims the body and appends the overlay, an open
      running block reads interrupted; 'stalled' is modelled but has no
      producer until delta-silence detection lands.
- [x] **The command palette** (#157) — the design README's §6 in DOM, over the
      #132 store. What it offers is built purely from what the browser holds:
      blocks from the attached tabs' `GridView.blocks` (the "N hosts searched"
      count states exactly that set), sessions from the open tabs plus the
      directory deduped on the full (host, session) pair, hosts from the
      launcher's list, and only actions that work — layout toggle and theme
      switches; no settings/profiles rows advertised before they exist.
      Ranking (`palette/rank.ts`) pins the group order Blocks → Sessions →
      Hosts → Actions whatever the match quality says — the palette is
      primarily command history — with subsequence + recency scoring within a
      group and recents on the empty query; provenance ages render only when
      the host stamped a timestamp. ⏎ resolves per row kind in a pure
      `runTargetOf`: a block re-runs through the terminal's own ⌘⇧R prompt
      gate and only on the active tab (a background tab activates and does
      nothing destructive), a session activates or opens, a host takes the
      launcher's create path, an action dispatches. While open the palette
      owns the keyboard (chords are claimed but only the toggle acts, and Tab
      is trapped — the hidden input is the dialog's only tab stop, so focus
      cannot walk out to the pty behind the scrim), and dismissal restores
      focus to whatever held it — the terminal textarea.
      The footer omits ⇧⏎ run-on-host: no host-chooser hook exists yet, and a
      dead advertised chord reads as broken.
- [x] **The fleet is a card grid, and the themes get a gallery** (#158) — the
      design README's §7–§8 in DOM (docs/design/client-ui/README.md carries the
      measurements). Host cards render from a pure `fleet-model` view-model
      whose rule the tests pin: absent fields are omitted, never faked — the
      registry carries enrolment facts only, so path/latency and the tunnel
      pill stay #148, wake-over-LAN stays #146, a session count appears only
      when something real supplies one, and the key row is the enrolled public
      key itself (ADR-006), head+tail truncated. `/themes` renders the five
      built-ins as live previews in each theme's OWN colours — the one place
      inline colour is correct, because a preview painted in the page theme
      previews nothing — over a swatch strip read from `resolveTerminalPalette`
      in index order, never re-typed; clicking calls the theme store (#133) and
      the active ring follows the theme actually applied. No import card
      (#147); a nav affordance to reach `/themes` is still open.
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
- [x] **The directory Worker**: host ids, labels, last seen. **No session
      state.** → ADR-006. Landed with the account (#53): `hosts` and `devices`
      keyed on the Ed25519 public key itself, so enrolment is a signature
      rather than a claim.

      **It holds no *endpoints*, and after the relay it never will.** ADR-006
      wrote the row as "host ids, labels, last-seen endpoints", which assumed
      an away client dials an address the host published. Under dial-back there
      is no such address — the host reaches *out*, and the directory answers
      "which machines are mine and are they up", not "how do I route to one".
      The LAN still discovers endpoints, and discovers them locally over mDNS,
      which is where they were always more accurate anyway. One fewer thing on
      someone else's disk.
- [ ] ~~Cloudflare Tunnel + Access per host~~ — **superseded by the relay**
      (#59, ADR-009). A tunnel terminates TLS at the edge, which is precisely
      what ADR-008 spent a protocol version making unnecessary; and it needs a
      per-host tunnel and an Access policy configured by hand on every machine,
      where a relay needs one outbound dial. The mandatory origin-side JWT
      validation that made a tunnel safe has no equivalent job left to do once
      the origin trusts nothing in the path at all. → M6.
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
reason — nothing is warm yet.

**Protocol 3 seals loopback too, and it cost 2µs.** Measured back to back on one
machine rather than against the table above, because that was a different build
on a different day and comparing across them would attribute the machine to the
encryption. `attach --ping 500`, `--profile fast`, one warm-up run discarded,
six runs each:

| | p50 across six runs |
|---|---|
| plaintext (protocol 2) | 18, 16, 16, 17, 17, 17 |
| sealed (protocol 3) | 17, 19, 19, 19, 19, 19 |

**≈17µs → ≈19µs.** Two ChaCha20-Poly1305 operations per round trip, on frames
small enough that the per-record setup dominates and the per-byte rate does not
appear at all. ADR-008 argues why loopback is sealed anyway; this is what the
argument costs. This is a **floor** for input-to-paint, not the
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

Device enrollment, the directory Worker, the web client. → WS-G, WS-H.

This milestone originally opened with *"Cloudflare Tunnel + Access per host"*.
That is now M6's relay instead, for the reasons in WS-H's row and ADR-009 — the
short version being that a tunnel terminates TLS at the edge, which is the one
thing ADR-008 spent a protocol version making unnecessary.

**Actors are the control plane, never the data plane**, and they run **locally**
on each host. → ADR-005, ADR-006.

- [ ] Bun single-file sidecar hosting `@sigx/actors`, spawned as a child of the
      daemon, length-prefixed msgpack over stdio. Never in the PTY hot path.
- [ ] Device enrollment: non-extractable Ed25519 key, desktop approval modal
      with a matching code.

      **The browser's half of the key has landed.** A `ClientSigner` seam in
      `@zesterm/auth` — `seedSigner` over a seed this process holds, or
      `webCryptoSigner` over a non-extractable `CryptoKey` in IndexedDB, and
      the handshake cannot tell which. It could not be a wrapper: `@noble`
      needs the raw 32-byte scalar and a non-extractable key will never yield
      one, so signing goes through `crypto.subtle` and is **async**, which
      made `answerChallenge`, `HandshakeDriver.onMessage` and both clients
      async with it. *Verification* stayed synchronous on `@noble` on
      purpose — it touches only public keys, and that is what preserves the
      ordering the whole handshake rests on: the host proves itself before
      anything is signed. Two hazards the async path introduced are guarded
      and tested: a host that pipelines two handshake messages must not be
      served out of order (it would `attach` before its `auth` reached the
      wire), and a `subtle` signature that settles after its connection
      dropped must not be replayed onto the next one.

      **No silent rotation**: a device that already has
      `zesterm.device-seed.v1` keeps it. A new key means a new `ClientId`
      means every daemon in the fleet re-prompts a device the person already
      approved, which is exactly how people learn to approve without reading.
      Migration needs enrolment to carry the old key's blessing to the new
      one. Ed25519 support is feature-detected by *generating and signing*,
      not by name, and the kind of key backing the device is surfaced to the
      UI — a browser on the fallback is working, not secure, and the screen
      says so.
- [x] Host enrollment: `zest-daemon --enroll <code>` signs a code carried from
      the account's devices screen with the host key, and keeps the token it is
      given beside the private key in the OS credential store — `--logout`
      forgets it, `--account` says what is held. Foreground flags, because a
      detached daemon has no terminal to be handed a one-shot code on.

      **The seam was built before the transport, and it paid.** The signing,
      the JSON, what counts as a refusal and where the token goes are the parts
      that are wrong in ways nobody notices, and none of them needs a socket;
      they landed against an injected `ControlPlane` while `NoHttpClient` held
      the hole open with an error naming the crate that did not exist yet.
      `HttpsControlPlane` — `zest_cloud::http` over `zest_cloud::tls` — then
      replaced it with no change to that logic and none to the tests around it;
      the only test that went was the stub's own.

      Not claimed: no enrolment has been made against the deployed Worker.
      Both ends are tested against the shared preimage rather than against each
      other, and the first live claim is still the first live claim.
- [ ] Attach tickets (30s TTL, single use) minted by the actor.

## M5 — phone, AI, end-to-end encryption

- [ ] Lynx app. **Blocks-first, not grid-first** — a phone is excellent at
      lists, and you drop into grid view only when `alt_screen` is true, which
      the host already reports. Sticky `Ctrl` toggle, local history from the
      block index, long-press to re-run.

      **Designed** → `docs/design/phone/README.md`, written against what the
      web slice proved: the phone reuses `@zesterm/proto`/`auth`/`client`/
      `theme` unchanged over a `lynx-websocket` `Dial`, reads the same
      `SessionDirectory` live via `socketTransport({connect})`, and keeps a
      persistent device key in secure storage from day one. The one open piece
      is grid rendering on Lynx (no canvas package at 0.26); blocks-first is
      what makes that deferrable.
- [x] **E2E encryption of the data plane** — **done, protocol 3** (#55). The
      only mitigation that survives a hostile relay, and it landed *before* the
      relay so Cloudflare is a dumb pipe the day that ships rather than a
      trusted party demoted later.

      **Not Noise IK, and not HPKE.** Signed ephemeral X25519 instead: each side
      puts a DH public key into the transcript both were already signing, so the
      existing Ed25519 signatures certify it — no certificate type, no static DH
      key to store or rotate, and forward secrecy for free. Two implementations
      of a *framework* is far more surface than two of one handshake, and `snow`
      has no browser twin. ChaCha20-Poly1305 over AES-GCM because
      `crypto.subtle` is async and the browser's decode path is not. ADR-008 has
      the full argument and what it supersedes.

      Sealed on loopback too, and **measured: p50 ≈17µs → ≈19µs** (see above).
      Both implementations are pinned to `fixtures/handshake.json`, which
      carries the transcript, its hash, both directional keys and records
      straddling the 2²⁴ ratchet — it caught a full-HKDF-vs-Expand mistake in
      the second implementation on first use.
- [ ] `AiActor` over sigx `streams:`, per-block consent, redaction.

---

## M6 — the relay, and the fleet from anywhere

**Win condition:** *the daemon on the Mac, a laptop tethered to a phone, the
deployed URL — `vim`, a resize, close the lid, reattach from a second device.*
Two genuinely different networks. Nothing short of that proves it, because
every part of this that can be wrong is wrong only when there is no route
between the two machines. → #59, ADR-009.

The design is settled and the arithmetic is checked; see ADR-009 for dial-back
versus a mux, one object per host, why the relay is a second Worker, and the
three facts about Cloudflare that changed after #59 was written.

- [x] **The daemon's WebSocket client can dial something that is not a daemon.**
      `client::connect_to` takes halves that are **already split** — a TLS
      stream can be neither cloned nor split, so whoever owns it says how — plus
      a path and `Host` of its own, caller-supplied headers, and a subprotocol.
      Each of the last three is something the relay needs and a daemon port does
      not; the ticket travels on `Sec-WebSocket-Protocol`, so the header values
      come from a control plane and are validated against the HTTP token charset
      rather than trusted. Beside it a *message*-oriented reader, because the
      object's free keepalive is defined over string members and that puts text
      frames on the control link. `connect` is now a wrapper over it and the
      request it sends is byte-identical.
- [x] **The public-port hardening is one object, and it can key on more than an
      address.** `Gate` — the mid-handshake cap and the per-peer failure limit —
      is built once by the process and handed to every transport, where each
      accept loop used to make its own. That is a **behaviour change**: the LAN
      port and the WebSocket port had 32 mid-handshake slots each and now share
      32, which is what ADR-009 needs (a relay stream is a socket, and must
      count) and what the resource actually is (threads), at the cost that a LAN
      flood can now crowd out a relay attach.

      The limiter is keyed on an opaque `PeerKey` rather than a string, because
      behind a relay every connection carries the relay's address and five
      failures would let one hostile peer deny the whole fleet for a minute at a
      time. And the watchdog cuts through a `Cut` rather than a `TcpStream`,
      since a TLS stream is not one and cannot be cloned into one.

      **The cut did not work on Windows, and nothing said so.** `Cut for
      TcpStream` was `shutdown(Both)`, which on Winsock does not unpark a reader
      already sitting in `read` — so `serve`'s thread never returned, and the
      `Countdown` it held kept one of the 32 now-shared slots for ever. Thirty-two
      silent connections would have made the daemon deaf to every new client,
      from anywhere, with no error and every call reporting success. Both
      watchdog tests injected a fake `Cut`, so they proved the timer fires and
      nothing about whether a real socket comes back; the integration test
      watched the *client* see EOF, which `shutdown` does deliver on every
      platform. The fix is #94's, in this module's shape: a `Severable` that
      arms a one-second read poll before any reader can park and swallows an
      elapsed poll, so only a real cut ends a read — and stands the poll back
      down once the handshake completes, since the 0%-idle property should not
      pay for a watchdog that is no longer watching. → #99.
- [ ] **The relay Worker and its Durable Object.** A control link the daemon
      parks, an attach ticket the browser carries on `Sec-WebSocket-Protocol`
      (not the query string — a secret in a URL lands in referrers, edge logs
      and history), and a pipe that comes into existence when the two meet.

      **Nothing in instance fields**, because the object is evicted between
      messages; tags and attachments are what survive. The guard is the test
      suite run twice, the second time constructing a new instance before every
      handler call. → ADR-009.

      **The ticket landed first**, on both sides: the account service mints one
      from a session cookie at `POST /api/relay/ticket`, and `GET /v1/attach`
      verifies it statelessly — signature, audience, expiry, and the room in the
      URL against the room in the signed payload — before any Durable Object is
      touched, so a refused attach costs no wake-up. It is signed by the account
      service's key alone and by no key in the fleet, and verified against a
      *list* of public keys, because rotating a single one would mean deploying
      two Workers at the same instant.

      **The control link landed second**, on the Worker's side: `GET /v1/control`
      challenges with a 32-byte nonce and the relay's own public key, and a
      daemon answers with an ordinary `Role::Host` + `Purpose::Auth` signature
      over that nonce — the `zesterm-sig-v1` preimage that already existed, so
      there is nothing new for a daemon to implement. `zest-mesh`'s
      `the_host_auth_signature_is_stable` emits the vector the TypeScript pins,
      because a byte of drift between two implementations otherwise arrives at
      bring-up as a daemon that is refused and a Worker that says the signature
      is bad, with neither able to say which moved.

      **The nonce lives in the attachment, not in a field**, and that is the one
      worth naming: it is the handshake rather than the pairing table, the gap
      between `challenge` and `hello` is a network round trip, and a nonce in a
      field passes every test written against a single live instance while
      refusing every real daemon after the first idle moment. The suite runs
      twice for it — and both halves of the guard were checked by writing the
      bug: the nonce in a field fails nine of sixteen tests in the evicting run
      and none in the live one, while "is a host present" in a field is the
      opposite and only the `getWebSockets` call count catches it. Keepalive is
      the object's free auto-response, the `hosts` lookup is cached 60s in the
      object's own storage, `last_seen_at` is written on connect, and the data
      path writes no storage at all. An attach that finds nobody home earns
      `4404`, and one whose host is present and does not dial back earns `4502`,
      which are different problems and only one of them is the user's.

      **The pipe landed third** (#110): the object mints an id, sends `open`
      down the parked link, the daemon dials `/v1/pipe` for that id alone, and
      after that the class is a byte pump. The 101 is written only once the pipe
      has two ends, because a browser whose `open` fired early sends its `Hello`
      into a room with nowhere to put it.

      **And the ticket is now spent once.** The `jti` has been in the payload
      since the mint; the room records it in `ctx.storage` before it does any
      dial-back work, so a captured ticket buys nothing even inside its thirty
      seconds and a replay costs the host neither an `open` frame nor a timer.
      The check cannot live at the edge — the Worker holds no state and two
      colos would not see each other's spent ids — which makes this the one
      claim verified in one place and spent in another. An alarm sweeps the set,
      because one that only grows is a leak, and it re-arms only while something
      is left: a room whose last browser detached hours ago has to go quiet
      completely. → ADR-009, `cloud/packages/relay/src/room/replay.ts`.

      Provable **before any Rust exists**: a Node script holding the control
      link with a real host key, dialling a real `zest-daemon --listen-ws`,
      proves the browser, the ticket, the object, the pipe, the `zest-proto`
      handshake and the sealed channel end to end. Only the daemon's own
      outbound leg needs TLS.
- [x] **`zest-cloud`, and the workspace's first TLS stack.** The one crate that
      owns rustls and HTTP, with `cargo xtask check-deps` growing a boundary
      that keeps them out of every crate that crosses to wasm or to a client.

      **The crate and the fence landed first, empty** (#65): a module doc, no
      dependency, no function, and nine names added to seven forbidden lists.
      The fence is only worth having if it predates the thing it fences, so the
      commit that adds rustls is the one that proves it works rather than the
      one that discovers it does not. Note what it does *not* claim —
      `zest-app` depends on `zest-daemon` which will depend on `zest-cloud`, so
      rustls reaches the desktop binary by design; the property is one owner,
      and none in the portable crates.

      **The hard part is not TLS, it is splitting it.** `serve()` needs two
      independently owned halves and a `rustls::StreamOwned` can be neither
      cloned nor split, while a mutex reproduces exactly the deadlock `ws.rs`
      documents. The answer holds the connection lock across no syscall at all
      — and the constraint that rules out the obvious two-mutex version is that
      **TLS records carry an implicit sequence number**, so two threads racing
      to the socket reorder them and the peer fails the MAC. That is a rare,
      unreproducible disconnect under load, which is the whole bug class this
      milestone is trying not to ship.

      **`TlsDuplex` is the answer**, and the fence held: rustls arrived in
      `zest-cloud` and `check-deps` stayed green with no list edited. `ring`
      rather than the default `aws-lc-rs`, which needs CMake on all three
      runners and NASM on Windows and CI installs neither. Both root sources
      are compiled in and chosen at runtime, because a corporate middlebox
      needs the platform verifier and a minimal container has no trust store
      for it to find. Cold-build cost, measured rather than guessed: **+18s**
      for the crate and its dependencies, and +21s more for the `rcgen`
      dev-dependency the tests mint a certificate with.

      Beside it, an HTTP POST in about a hundred lines rather than a client
      crate: one exchange, `connection: close`, a body read by
      `content-length`. Chunked transfer-encoding is **detected and refused by
      name** rather than half-decoded, because a chunk-size line read as a body
      reaches `--enroll` as a token that is really a hex number.
- [x] **`--enroll` over a real HTTP client.** `NoHttpClient` is gone;
      `HttpsControlPlane` posts the signed claim over `zest_cloud::http`, and
      `zest-daemon` is the first crate to depend on `zest-cloud`.

      **Which proves the half of the fence that was vacuous.** #68 said rustls
      has one owner and that the portable crates have none — but nothing
      depended on `zest-cloud`, so only the first claim had ever been tested.
      `check-deps` still reports all 9 boundaries hold with no list edited, and
      `cargo tree -p zest-app -e normal -i rustls` shows the one path it
      predicted: `zest-cloud → zest-daemon → zest-app`. rustls in the desktop
      binary is the design, not a leak.

      The URL splitting lives in `zest-cloud` beside `HTTPS_PORT` rather than
      in the caller, because "an absent port means 443" is a statement about
      the dialler; the argument is in `Endpoint`'s doc comment. It refuses
      rather than guesses at `http://`, userinfo, a bracketed IPv6 literal, a
      fragment and a query with no path in front of it — the first of those
      would post a bearer token in the clear, and each of the rest addresses
      something nobody chose.
- [x] **`--relay`: the daemon dials the relay itself.** The last large piece,
      and it adds no protocol — it composes `TlsDuplex`, `ws::client`, the
      `Gate`, the `Watchdog` and `serve_lan` into `zest-daemon/src/relay.rs`.
      A control link is parked with a `Role::Host` + `Purpose::Auth` signature
      over the room's nonce, and each `open` becomes a **second connection** to
      `/v1/pipe`, handed to `serve_lan` exactly as a device on the LAN is. One
      pipe is one socket, which is what lets the handshake watchdog cut a
      logical stream. Refuse-not-degrade, like `--listen-ws`: a relay that will
      not come up is a log line, and loopback keeps serving. → ADR-005.

      **The transport is injected** — `Fn(&str, u16) -> io::Result<Wire>`,
      defaulting to TLS — because the real relay is a Worker and without the
      seam this leg has no test at all. `tests/relay.rs` runs the whole of it
      against an in-process fake relay built on `tungstenite`, an independent
      RFC 6455 implementation rather than a mirror of `ws.rs`: challenge,
      signature, `open`, dial-back, and a real `zest-proto` session at the end
      of it. Every pipe takes a `Gate` slot and sets the 30 ms floor; neither
      goes through `accept_hardened`, which is inbound and `TcpListener`-shaped.

      **It has been run against the real thing**, which is what the fake relay
      cannot prove — see `cloud/README.md`. It also found the one bug no test
      here had: the redial ladder reset only when a parked link closed
      *cleanly*, and almost nothing closes cleanly. Restarting the Worker left
      the daemon waiting out the twenty seconds three earlier refusals had
      built, with a healthy relay already listening. `LinkEnd` now carries
      "did it park" beside "how did it end", because a `Result` cannot.

      **Two gaps stated rather than closed.** The relay's `relay_key` is read,
      logged and pinned to nothing — the sealed channel inside a pipe is what
      makes that survivable, and the Worker cannot yet *prove* the pin either
      (`env.ts` says so). And a relayed pipe takes a mid-handshake slot but
      feeds no rate limiter, because the only key available is the edge's
      address and one hostile peer on it would lock out the whole fleet — the
      hazard `PeerKey::Relay` already describes. Closing it needs a key the
      peer owns, which is its attach ticket, which the daemon never sees.
- [ ] **The web client learns a second data plane.** `DataPlane` grows a
      discriminant, a relay `Dial` mints its ticket before opening the socket
      (the seam stays synchronous — a failed mint is a dropped dial, and
      `SessionClient`'s backoff already handles that), and the cloud session
      list becomes a reactive store per host rather than an actors host in a
      browser tab.

      **The seam landed first**, with only the loopback implementation behind
      it: `clients/web/packages/app/src/directory-source.ts` is what
      `SessionList` reads, and it carries the argument against the two ways of
      giving a hosted tab an actor (`createHost` in the browser;
      `nodejs_compat` at the edge) so neither is rediscovered. The store half
      is blocked on two things that do not exist — the ticket endpoint
      `relay-dial.ts` injects around, and any notion of which hosts are
      online — so wiring the cloud branch today would replace an honest card
      with a list that reconnects for ever.
- [x] **The coalescing floor, with a test that asserts the message rate.** It is
      what keeps the object hibernating between keystrokes; unthrottled is
      ~1000 msg/s and an object that never sleeps. → ADR-009's arithmetic.

      **The daemon half landed first** (#72): `DaemonConfig::min_delta_interval`,
      honoured by the writer loop every transport shares, and **zero by
      default** so loopback and the LAN are unchanged. What remains is the relay
      transport setting it — and `relay.rs` now does, at 30 ms for every relay
      pipe and only for those, so the field has the one consumer it was written
      for. `pipe_config` is a function rather than two lines at the call site
      precisely so a test can hold it: the floor is invisible from a client's
      end, since it changes *when* deltas arrive and never what they say, so a
      line that quietly went missing would show up as a Cloudflare bill and
      nothing else.
      `tests/coalescing.rs` floods a session and asserts both halves: the
      message count stays inside one per interval, *and* the coalesced stream
      reconstructs the host's final screen exactly. Measured while writing it:
      1,294 updates without the floor, 5 with it, same final grid.

---

## Dogfooding

zesterm must correctly host `@sigx/terminal` TUIs — alt-screen, truecolor, raw
mode, resize, cursor and erase. Use `examples/showcase` and
`examples/claude-shell` from `C:\Dev\sigx\terminal\main` as acceptance content.

Theme `ui.*` tokens are `@sigx/terminal-zero`'s contract verbatim, so one theme
file styles zesterm's chrome *and* any sigx TUI running inside it.

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

| Crate | State |
|---|---|
| `zest-pty` | ✅ ConPTY, resize, shutdown protocol, `.vtrec` recorder, unix backend |
| `zest-core` | ✅ grid, scrollback, VT parsing, modes, OSC, palette — 🟡 block and subscriber seams frozen, unimplemented |
| `zest-font` | ✅ metrics, shaping, rasterization, system fallback, colour glyphs |
| `zest-theme` | ✅ tokens, OKLCH derivation, 5 built-ins, 4 importers |
| `zest-render-wgpu` | ✅ pipelines, atlas, offscreen resolve, selection — ⬜ gamma validation |
| `zest-config` | ✅ cascade, provenance, profiles, migrations, hot reload, JSON Schema |
| `zest-input` | 🟡 keys + SGR mouse, still inline in `zest-app` |
| `zest-app` | ✅ real window, real shell, sessions behind `SessionSource` |
| `zest-proto` | 🟡 wire types frozen; encoder, decoder and conformance corpus done — ⬜ framing |
| `zest-mesh` | ✅ Ed25519 identity, OS key store, mDNS discovery — ⬜ pairing, transports, cloud |
| `zest-daemon` | ⬜ skeleton |

385 tests. `--startup-probe` reports first paint at ~43ms against a 100ms budget.

---

# Workstreams

Bounded pieces that can run in parallel, each with its own issue. **Own only
your paths.** A job that seems to need another stream's files means the seam is
wrong — say so rather than reaching across.

Each has its own issue, self-contained enough to hand to an agent on its own.

| | Stream | Owns | Status | Issue |
|---|---|---|---|---|
| **A** | [Windows chrome, motion, polish](#ws-a) | `zest-app/src/{chrome,motion,platform}*`, `zest-render-wgpu/` | **Ready** — B unblocked it | [#5](https://github.com/zesterm/zesterm/issues/5) |
| **B** | [`zest-input`](#ws-b) | `crates/zest-input/` | Extracted; IME + Kitty open | [#2](https://github.com/zesterm/zesterm/issues/2) |
| **C** | [Unix PTY + macOS host](#ws-c) | `zest-pty/src/unix.rs`, macOS platform | **C1 done**, C2 open | [#3](https://github.com/zesterm/zesterm/issues/3) |
| **D** | [Linux host](#ws-d) | Linux platform + packaging | **Ready** — C1 landed `unix.rs` | [#9](https://github.com/zesterm/zesterm/issues/9) |
| **E** | [Command blocks](#ws-e) | `zest-core/src/blocks.rs`, OSC 133, shell integration | Ready | [#6](https://github.com/zesterm/zesterm/issues/6) |
| **F** | [`zest-proto` + `zest-daemon`](#ws-f) | `crates/zest-proto/`, `crates/zest-daemon/` | Ready — **the lead** | [#4](https://github.com/zesterm/zesterm/issues/4) |
| **G** | [Web client](#ws-g) | `clients/web/` | Ready (decoder) | [#8](https://github.com/zesterm/zesterm/issues/8) |
| **H** | [Mesh identity, discovery, transports](#ws-h) | `crates/zest-mesh/`, `cloud/` | **H1–H2 done**, pairing after F | [#7](https://github.com/zesterm/zesterm/issues/7) |

**Ordering that matters.** B before A — B extracts `zest-input` out of
`zest-app`, and A then builds chrome and motion in the same file; the other
order is a guaranteed conflict. C1 before D, since C1 writes the `unix.rs` that
serves both. F before H's transports, though H's identity work can start now.

**The insight worth naming:** *"make the Mac's terminals reachable"* (WS-C1 — a
`forkpty` backend behind an already-frozen trait, plus a headless daemon) is far
smaller than *"port the GPU terminal to macOS"* (WS-C2 — Metal,
`NSVisualEffectView`, traffic lights, CoreText). C1 unlocks the fleet; C2 can
trail by weeks.

### WS-A — Windows chrome, motion, polish

M1 steps 11–13. Owns `zest-app/src/chrome/`, `motion/`, `platform.rs`, and
`zest-render-wgpu/`. Consumes `SessionSource`.

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

Extraction from `zest-app` collides with WS-A, so **land it early and small**.

- [ ] Move key/mouse encoding out of `zest-app/src/{input,mouse}.rs` into the
      crate. Behaviour-preserving; the 39 existing tests move with it.
- [ ] IME and dead keys via winit `Ime`.
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

**C2 — the GPU app on macOS (can trail)**
- [ ] Metal surface, CoreText fallback verification, `NSVisualEffectView`.
- [ ] **Do not go borderless on macOS.** It costs traffic lights, native
      full-screen, Sequoia tiling and accessibility, and gains nothing over
      `titlebar_transparent` + `title_hidden` + `fullsize_content_view`. The
      traffic-light inset is not a constant — recompute on full-screen changes.

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

- [ ] OSC 133 A/B/C/D, OSC 7 (cwd), OSC 633 (VS Code alias) → `BlockIndex`.
      The markers are already recognized and ignored, so they never reach the
      grid as garbage.
- [ ] Shell integration for PowerShell/bash/zsh/fish with consented
      auto-install. oh-my-posh can emit these.
- [ ] Block folding, re-run, copy-output in the native UI.

> The strongest reason this project owns its grid rather than depending on
> `alacritty_terminal`: blocks need new row fields, a side index surviving
> scrollback eviction, and OSC handler hooks. Each would be a fork of an
> explicitly-unstable crate.

### WS-F — `zest-proto` + `zest-daemon`

**The lead stream.** Wire types are frozen; encoding and the daemon are not.

- [ ] Implement `ChangeSource` on `Terminal` — the `update_for` rule is already
      settled and tested.
- [ ] Delta encoder: attribute interning, `SCROLL` before `ROW`, explicit cell
      counts. MessagePack envelope; do **not** msgpack individual cells.
- [ ] `ts-rs` codegen with golden-fixture contract tests in CI.
- [ ] `zest-daemon`: session ownership, loopback transport, SQLite scrollback.
- [ ] `zest-app` gains a daemon-attached `SessionSource`. **Find-or-spawn goes
      in the slot the shell spawn occupies today** — after the window is
      visible, overlapping driver init. `--startup-probe` must still pass.
- [ ] **Conformance corpus**: replay the `.vtrec` files, apply deltas in the
      client, assert cell-for-cell identity at every frame. This is the spine.
- [ ] Chaos-resync 10,000 times at random disconnect points.

### WS-G — Web client

Owns `clients/web/`. The decoder can be built against golden fixtures before the
daemon exists.

- [ ] TypeScript delta decoder against the `ts-rs` bindings.
- [ ] Grid renderer. **`@sigx/terminal` cannot be reused** — it paints TSX *to* a
      TTY, which is the inverse of what a web client needs.
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
- [ ] Pairing: desktop approval modal with a matching code, signed nonce on
      every `Hello`. A stolen session alone must not get a shell.
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

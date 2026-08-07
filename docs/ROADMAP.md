# zesterm roadmap

The durable plan. Mirrored as tracking issue
[#1](https://github.com/zesterm/zesterm/issues/1), but **this file is the source
of truth** — update it in the same commit as the work it describes, then refresh
the issue.

## The goal, and why the architecture looks like this

A terminal in the Warp performance class — GPU-rendered, low input latency,
deeply themable — on Windows, macOS and Linux. Beyond being a good local
terminal, the end goal is to **reach this machine's shells from the internet**,
driving them from a browser and a phone.

That end goal dictates everything. A terminal built as a monolithic GUI app
cannot grow a remote head without a rewrite, because its state lives inside the
renderer. So `zest-core` is headless and knows nothing about pixels, and the
native app is merely the first frontend. See [ARCHITECTURE.md](ARCHITECTURE.md)
for the decisions that are expensive to reach and cheap to accidentally undo.

## Status

| Crate | State |
|---|---|
| `zest-pty` | ✅ ConPTY, resize, shutdown protocol, `.vtrec` recorder |
| `zest-core` | ✅ grid, scrollback, VT parsing, modes, OSC, palette |
| `zest-font` | ✅ metrics, shaping, rasterization, system fallback, colour glyphs |
| `zest-theme` | ✅ tokens, OKLCH derivation, 5 built-ins, 4 importers |
| `zest-render-wgpu` | 🟡 pipelines + atlas + offscreen resolve; renders offscreen, no window yet |
| `zest-input` | 🟡 keys live in zest-app for now; no mouse, no Kitty protocol |
| `zest-config` | ⬜ |
| `zest-app` | 🟡 real window, real shell; input is basic |

179 tests. `cargo run -p zest-app --example headless` is a working terminal
without a window.

---

## Milestone 1 — a terminal worth using on Windows

**Win condition:** *"I used this instead of Windows Terminal for a week and
didn't want to switch back."* Not feature parity with WezTerm.

- [x] **0. Toolchain.** VS Build Tools. The Windows SDK alone is not enough —
      `libstd` needs the MSVC CRT for `__CxxFrameHandler3` and `__chkstk`.
- [x] **1. Workspace + enforced boundaries.** `cargo xtask check-deps` fails the
      build if `zest-core` grows a dependency on wgpu/winit/windows/tokio.
- [x] **1.5. Transparency probe.** Settled premultiplied alpha before any shader
      existed. → ADR-003.
- [x] **2. `zest-pty`.** ConPTY with the shutdown protocol, plus `.vtrec`
      recording for the replay corpus.
- [x] **3. `zest-core`.** Ring storage, 16-byte `Cell`, deferred wrap, scroll
      regions, alt screen, CSI/SGR/OSC. Absolute line IDs and a sequence counter
      for M3.
- [x] **4. `zest-font`.** swash + fontique. Integer physical-pixel metrics,
      shaping, system fallback, COLR/CBDT colour glyphs. PNG dump for diagnosis.
- [x] **4b. `zest-theme`.** The five built-in themes, OKLCH derivation of `ui.*`
      tokens, and importers for iTerm2 / Windows Terminal / base16 / Alacritty.
- [ ] **5. `zest-render-wgpu`.** Three pipelines, one render pass:
      - [x] SDF rects — cell backgrounds, selection, cursor, *and* all window
            chrome. One pipeline for everything rectangular.
      - [x] Glyphs — instanced quads from a dual atlas (R8 masks + RGBA colour),
            `etagere` shelf allocator, generation-based bulk eviction.
      - [x] Decorations — underline, undercurl, strikethrough.
      - [x] `Rgba16Float` offscreen + resolve pass (gamma, premultiplication in
            encoded space, free OS-driven repaints).
      - [x] Offscreen PNG harness (`--example render_dump`), runs on a fallback
            adapter so it works in CI.
      - [ ] Selection rendering (needs the selection model from step 8).
      - [ ] Validate gamma side-by-side against Windows Terminal. **Do not defer
            this** — it ships broken constantly and reads as "looks slightly off".
- [x] **6. `zest-app` — the moment.** winit, PTY thread, fair mutex, first
      window with real output. **A working terminal.**
- [ ] **7. `zest-input`.** Currently inline in `zest-app` and covers keys,
      modifiers, DECCKM, Alt-as-ESC and F-keys. Still to do: extract the crate,
      IME/dead keys, SGR-1006 mouse, and the Kitty keyboard protocol behind a
      flag — plan that now, retrofitting hurts.
- [ ] **8. Selection + clipboard.** Absolute coordinates so selection survives
      scrolling and eviction. Wrapped-line copy must not insert a newline.
- [ ] **9. Scrollback + scrolling.** Wheel, Shift+PgUp/PgDn, alt-screen wheel →
      arrow keys so `less` and `man` scroll.
- [ ] **10. `zest-config`.** Cascade, profiles, provenance, migrations, hot
      reload with invalidation classes, JSON Schema export.
- [ ] **11. Window chrome + motion.** Borderless window, GPU-drawn titlebar and
      tabs, per-OS backdrop, springs, smooth scroll, `reduce_motion`.
- [ ] **12. Polish.** Title, DECSCUSR cursor styles, font zoom, DPI changes.
- [ ] **13. Performance validation.** vtebench, >500 MB/s throughput, <2 ms CPU
      frame, <10 ms keypress→pixel, **0% GPU when idle and animations settled**.

### Sequencing risks

Three things are cheap now and force a rewrite later. They are already decided —
do not undo them:

1. **Premultiplied alpha + offscreen resolve.** Retrofit = rewrite every shader.
2. **`GlyphInstance` carries absolute physical-pixel position and RGBA colour**,
   not `(row, col)` and a palette index. Retrofit = rewrite the glyph pipeline
   and every call site. This is what lets chrome text, tab titles, the command
   palette and block headers all share the grid's atlas.
3. **`render(&[Viewport], &Chrome)`** even though M1 always passes one viewport.
   Retrofit = restructure the render loop for M3 panes.

### If M1 must not slip, cut in this order

Theme importers → macOS/Linux chrome parity → cursor smear (ship the spring,
default `trail = "none"`) → theme gallery UI (a CLI is enough).

**Do not cut:** the three items above, `PaletteSnapshot` in core, or the
settings `diff()`/`Invalidation` machinery.

---

## Milestone 2 — command blocks

OSC 133 A/B/C/D plus OSC 7 (cwd) and OSC 633 (VS Code alias), parsed in
`zest-core` and attached to absolute line ranges. Shell integration for
PowerShell/bash/zsh/fish with consented auto-install — oh-my-posh can emit these.
Block folding, re-run, copy-output.

The markers are already recognized and ignored, so they never reach the grid as
garbage.

> This is the strongest reason not to depend on `alacritty_terminal` wholesale:
> blocks need new row fields, a side index surviving scrollback eviction, and OSC
> handler hooks. Every one is a fork of an explicitly-unstable crate.

---

## Milestone 3 — "my phone on my wifi drives a terminal"

No Cloudflare, no actors yet. Prove the protocol in isolation.

**The crux: ship grid deltas, not raw PTY bytes.** Raw bytes (ttyd/wetty) are
simpler and proven, but lose on four counts — resync is unsolvable on a
reconnecting mobile link, deltas coalesce and bytes don't, blocks are semantic
rather than textual, and two VT emulators means two truths. This is mosh's State
Synchronization Protocol plus a semantic block layer. → ADR-004.

- [ ] `zest-core` subscriber API: `subscribe` / `delta` / `keyframe` / `ack` /
      `scrollback`. Absolute line IDs and the sequence counter already exist.
- [ ] `zest-proto`: binary WebSocket, MessagePack envelope, packed delta ops
      (`SCROLL`, `ROW`, `CURSOR`, `ERASE`, `ATTRDEF`, `SBPUSH`, `IMAGE`).
      Attribute interning and `SCROLL`-before-`ROW` carry most of the win.
- [ ] `ts-rs` codegen with golden-fixture contract tests in CI.
- [ ] `zest-daemon`: axum + tokio-tungstenite, SQLite scrollback.
- [ ] A minimal SignalX web client.
- [ ] **Conformance corpus**: replay the `.vtrec` files, apply deltas in the TS
      client, assert cell-for-cell identity at every frame. This is the spine.
- [ ] Chaos-resync 10,000 times at random disconnect points.
- [ ] Bun `--compile` spike for the M4 sidecar — one day, done early.

---

## Milestone 4 — anywhere, over Cloudflare, with blocks

**Actors are the control plane, never the data plane**, and they run **locally**.
→ ADR-005.

- [ ] Bun single-file sidecar hosting `@sigx/actors`, spawned as a child of the
      daemon, length-prefixed msgpack over stdio. Never in the PTY hot path.
- [ ] `SessionActor`, `DaemonActor`, `WorkspaceActor`.
- [ ] Cloudflare Tunnel + Access. **Origin-side JWT validation is mandatory** —
      the origin never trusts the tunnel.
- [ ] Device enrollment: non-extractable Ed25519 key, desktop approval modal
      with a matching code. A stolen Access session alone must not get a shell.
- [ ] Attach tickets (30s TTL, single use) minted by the actor.
- [ ] Remote access **off by default**, persistent indicator, audit log.

---

## Milestone 5 — phone, AI, end-to-end encryption

- [ ] Lynx app. **Blocks-first, not grid-first** — a phone is excellent at lists,
      and you drop into grid view only when `Modes.altScreen` is true. Sticky
      `Ctrl` toggle, local history from the block index, long-press to re-run.
- [ ] **E2E encryption of the data plane** (Noise IK / HPKE, keys bound to device
      enrollment). The only mitigation that survives a hostile relay — first
      class, not a stretch goal.
- [ ] `AiActor` over sigx `streams:`, with per-block consent and redaction.
- [ ] Edge `DeviceActor` for push only: `new_sqlite_classes` (irreversible),
      `define: __DEV__`, `nodejs_compat`, export a factory not an instance.

---

## Dogfooding

zesterm must correctly host `@sigx/terminal` TUIs — alt-screen, truecolor, raw
mode, resize, cursor and erase. Use `examples/showcase` and
`examples/claude-shell` from `C:\Dev\sigx\terminal\main` as acceptance content.

Theme `ui.*` tokens are `@sigx/terminal-zero`'s contract verbatim, so one theme
file styles zesterm's chrome *and* any sigx TUI running inside it.

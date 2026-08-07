# Architecture decisions

Decisions here were expensive to reach and are cheap to accidentally undo. Each records
what was chosen, what was rejected, and *why* — so a future "simplification" has to argue
with the reasoning rather than rediscover it.

---

## ADR-001 — `zest-core` is headless and knows nothing about presentation

**Status:** accepted

The end goal is driving local shells from a browser and a phone. A terminal whose state
lives inside its renderer cannot grow a remote head without a rewrite. So `zest-core` owns
PTY-fed VT parsing, the grid, and scrollback, and depends on none of `wgpu`, `winit`,
`windows`, or `tokio`. It builds for `wasm32-unknown-unknown`.

`cargo xtask check-deps` enforces this in CI. A boundary that isn't checked decays within a
month.

---

## ADR-002 — Colors are stored unresolved; the seed palette and the live palette are separate

**Status:** accepted

Cells store `Color::Default | Indexed(u8) | Rgb(..)` exactly as SGR delivered them. Nothing
is resolved to RGB at parse time.

But escape sequences both query and mutate real color values — `OSC 4;n;?`, `OSC 10/11/12;?`
(TUIs query the background to detect light vs dark), `OSC 4;1;#ff0000`, and the
`OSC 104/110/111/112` resets. So core does need concrete RGB. The division:

> **`zest-theme` owns the seed palette. `zest-core` owns the live one.**

`PaletteSnapshot` is plain data defined in core with no theme semantics, so this is not a
dependency on `zest-theme`.

**Rejected:** resolving at parse time — a theme change would have to rewrite all of
scrollback. **Also rejected:** letting the theme own the mutable palette — then `OSC 4` and
a theme reload fight over the same state.

---

## ADR-003 — Every shader outputs premultiplied alpha

**Status:** accepted — settled empirically by `cargo run -p zest-render-wgpu --example alpha_probe`

This had to be decided before any pipeline was written, because retrofitting premultiplication
means rewriting every shader.

Probe results on the development machine (Windows 11, hybrid graphics):

| Backend | Adapter | `alpha_modes` |
|---|---|---|
| Vulkan | NVIDIA RTX 3080 Ti Laptop | `Opaque`, **`PreMultiplied`** |
| Vulkan | Intel Iris Xe (iGPU) | `Opaque`, `Inherit` |
| Dx12 | NVIDIA RTX 3080 Ti Laptop | `Opaque` |
| Dx12 | Intel Iris Xe | `Opaque` |
| Dx12 | Microsoft Basic Render Driver | `Opaque` |

Two conclusions:

1. **DX12 cannot deliver per-pixel alpha through wgpu's ordinary surface path**, on any
   adapter. This confirms the underlying constraint: `IDXGIFactory2::CreateSwapChainForHwnd`
   only permits `DXGI_ALPHA_MODE_IGNORE`. Per-pixel alpha requires
   `CreateSwapChainForComposition` bound to a DirectComposition visual.
2. **Nothing offers `PostMultiplied`.** Both viable paths — native Vulkan `PreMultiplied` and
   a DirectComposition swapchain with `DXGI_ALPHA_MODE_PREMULTIPLIED` — want premultiplied
   output.

So the gating question has one answer regardless of which delivery mechanism ships:
**premultiply, everywhere, unconditionally.** Blend is
`One / OneMinusSrcAlpha` in every pipeline; no pipeline uses `SrcAlpha / OneMinusSrcAlpha`.

Note also that transparency on Windows is **adapter-dependent, not just backend-dependent** —
the discrete GPU can do it over Vulkan and the integrated one cannot, and a laptop runs on the
integrated GPU on battery. Choosing the backend based on a *settings* value would mean
recreating the device when the user changes opacity, which is not acceptable.

**Consequence, deferred but decided:** the universal Windows transparency path is a manually
managed DirectComposition presenter. It is isolated behind a `Presenter` trait in
`zest-app/src/platform/windows/present.rs` so the renderer never knows which path is live.
Until it exists, `window.opacity` is honored where the surface reports `PreMultiplied` and
otherwise forced to 1.0 with `Capabilities { transparency: Unsupported(..) }` reported to the
settings layer — **never silently ignored.**

**Related discipline:** window opacity applies **only to cells whose background is
`Color::Default`.** Applying it to every cell double-darkens, and makes cells with an explicit
background (dir colors, TUI panels, `@sigx/terminal-ui` boxes) see-through when they must not
be. Also: opacity never applies to glyphs. Translucent text is unreadable.

---

## ADR-004 — The remote protocol ships grid deltas, not raw PTY bytes

**Status:** accepted (design; implementation is M3)

The obvious alternative — stream raw PTY bytes and let the client run its own VT emulator, as
ttyd, wetty, and GoTTY do — is proven and much simpler. It was rejected on four counts:

- **Resync is unsolvable.** A VT stream is stateful and non-restartable. Drop 400 ms on mobile
  and there is no recovery short of replaying from session start; computing a safe replay
  checkpoint would require a server-side VT parser anyway. Mobile is a reconnecting-by-default
  environment.
- **Deltas coalesce; bytes do not.** The grid is a *state*, so a slow link can drop intermediate
  frames. `cat 1MB` is 1 MB of bytes versus roughly 3 KB of delta.
- **Command blocks are semantic, not textual** — raw bytes would mean parsing on both ends.
- **Two VT emulators means two truths.** Desktop and phone would drift on wcwidth, grapheme
  clustering, and `DECSTBM` edge cases. Clients never interpreting VT also structurally
  eliminates escape-sequence injection into client parsers.

This is mosh's State Synchronization Protocol plus a semantic block layer.

---

## ADR-005 — Session actors run locally, never at the edge

**Status:** accepted (design; implementation is M4)

`@sigx/actors` models terminal sessions, and it runs **on the user's machine** as a sidecar,
with Cloudflare acting purely as transport (Tunnel + Access). Actors are the **control plane**
— session list, cwd, block index, layout — never the data plane, which is a binary WebSocket
carrying grid deltas.

**Rejected: running session actors on Cloudflare Durable Objects.**

- Every keystroke would traverse two extra WAN legs, through whatever colo the DO was first
  created in — possibly neither near the user nor near their PC, permanently.
- ~1.15 M DO requests per session-day, plus all-day duration billing.
- Session actor state is the grid and the block index — shell output, commands, cwd, and
  anything secret echoed into a terminal — persisted on someone else's disk.
- The *local* terminal would depend on Cloudflare being reachable.

The "PC joins the Cloudflare actor cluster" variant is additionally **mechanically impossible**:
clustering requires hosts to advertise dialable addresses, and Cloudflare Tunnel is strictly
origin-initiated. There is no address to advertise. The host-to-host mount also runs no
policies, so it would expose an unauthenticated RPC surface.

Do not "simplify" toward edge actors.

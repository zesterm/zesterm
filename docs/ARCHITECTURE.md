# Architecture

The first half of this file says what the system **is** — the goal, the layers, the wire
path, the map of the repo. The second half records the decisions that were expensive to
reach and are cheap to accidentally undo: each ADR says what was chosen, what was
rejected, and *why*, so a future "simplification" has to argue with the reasoning rather
than rediscover it.

## The system

### The goal

A terminal in the Warp performance class — GPU-rendered, low input latency, deeply
themable — on Windows, macOS and Linux. And then the part that makes it worth building:
**every machine reachable from every device.** Not one machine exposed to the internet.
A fleet. The Mac's shell in a window on the Windows box. A Linux build watched from a
phone on a train. Sessions that outlive the window they were started in, picked up
wherever you are.

That goal dictates everything. A terminal built as a monolithic GUI app cannot grow a
remote head without a rewrite, because its state lives inside the renderer. So
`zest-core` is headless and knows nothing about pixels; the daemon owns sessions on
every machine; and the native app is a client of its own daemon exactly as the phone is
a client over the network.

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

### The layers

- **`zest-core` is headless** (ADR-001): VT parsing, the grid, scrollback and command
  blocks, with no dependency on `wgpu`, `winit`, `windows` or `tokio`, building for
  `wasm32-unknown-unknown`. One terminal implementation shared by every client instead
  of three that quietly diverge.
- **Every machine runs `zest-daemon`, and the daemon owns sessions** (ADR-007). A
  session outlives the window that started it; close the lid on the laptop, reattach
  from the phone — that only works because nothing about a session was ever the
  window's.
- **Everything else is a client** — the native app over the loopback socket, the
  browser over WebSocket, the phone, and the MCP agent (ADR-015). All speak the same
  protocol; the app gets no privileged in-process path.

### The wire path

PTY bytes are parsed by `zest-core` into the grid, scrollback and blocks on the host.
`zest-proto` encodes **grid deltas**, never raw VT bytes (ADR-004): protocol 3,
`Seq`/`Ack`, resync by keyframe, and everything after the handshake's `Challenge`
sealed end to end (ADR-008). The transports — loopback (named pipe / unix socket),
LAN TCP discovered over mDNS, WebSocket for browsers, and the relay both ends dial
out to (ADR-009) — carry identical messages. Clients apply deltas through `Applier` into a real `Terminal`
(Rust) or through `GridView` (TypeScript); `zest-core`'s conformance suite holds the
two reference decoders cell-for-cell equal against recorded sessions.

### The map

| Piece | Responsibility |
|---|---|
| `crates/zest-core` | VT parsing, grid, scrollback, command blocks. **No UI, no GPU, no process APIs; builds for wasm** |
| `crates/zest-pty` | ConPTY / `openpt` spawning, byte I/O, resize, hangup, `.vtrec` recording |
| `crates/zest-font` | Font discovery, shaping, CPU rasterization, fallback, colour glyphs, PUA |
| `crates/zest-theme` | Token schema, OKLCH colour math, built-in themes, scheme importers |
| `crates/zest-render-wgpu` | Glyph atlas, render pipelines, offscreen resolve |
| `crates/zest-input` | Key/mouse events to terminal byte sequences (Kitty CSI u, SGR mouse), mirrored in TS |
| `crates/zest-config` | Settings cascade, provenance, profiles, migrations, hot reload, JSON Schema |
| `crates/zest-app` | The `zesterm` binary: window, chrome, tabs, wiring — a client of its own daemon |
| `crates/zest-proto` | The delta protocol: encoder, `Applier`, framing, sealing, conformance fixtures |
| `crates/zest-daemon` | Session ownership and lifecycle; loopback, LAN, WebSocket and relay transports; the account client every device shares |
| `crates/zest-mesh` | Ed25519 identity, keystore, mDNS discovery, pairing, trust store |
| `crates/zest-cloud` | The one crate that owns rustls and HTTP: `TlsDuplex`, enrolment, relay dialling |
| `crates/zest-fleet` | What a machine in the fleet is, how its two sources merge into one row, and the one rule that picks how to reach it. Pure; no discovery, no sockets |
| `crates/zest-mcp` | Terminals as an agent's tools, over MCP on stdio (ADR-015) |
| `xtask/` | The gates: `check-deps`, `check-spawn`, plus schema / bindings / fixtures / web-export generation and checks |
| `schemas/` | The generated settings JSON Schema |
| `scripts/` | `worktree.mjs`, `apply-branch-protection.mjs`, `zesterm-dev` |
| `clients/web/` | The browser client — a pnpm workspace of ten packages; proto/theme/input are hand-written with no runtime deps, sigx appears only in control/sidecar/app |
| `cloud/` | A second, separate pnpm workspace: the Cloudflare Workers — hosted web client, relay Worker + Durable Object, directory/account service |

### Where web and cloud sit

The browser client consumes generated TypeScript bindings and applies deltas with
`GridView`; the settings schema, walked UI fields and built-in themes are exported into
it by `cargo xtask export-web`, so a Rust-only change to `zest-config` or `zest-theme`
can leave it stale — that is what `check-export-web` gates. `cloud/` is deliberately
dumb: the directory holds host ids, labels and last-seen and never session state
(ADR-006), and the relay routes sealed bytes it cannot read (ADR-008, ADR-009).
`cloud/README.md` covers why it is two Workers and a workspace of its own.

### Reflow

Resizing the width rewraps, rather than truncating and losing the text. A
*logical line* — rows joined by `wrapped`, which is what the program actually
printed — is rejoined and re-broken at the new width, so narrowing a window and
widening it again restores the screen exactly.

Rules that are not obvious and are load-bearing:

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
- **The grid is resized before the pty**, because on Windows a pty resize is
  answered by a full-viewport repaint and the reader parses it under the same
  lock. Told first, the pty sends back a screen laid out for the new size and it
  lands on a grid still at the old one — after which a perfectly correct reflow
  and a perfectly correct re-anchor still leave every block naming somebody
  else's text. The lock is released before the call rather than held across it;
  holding it is the `ClosePseudoConsole` deadlock again. (#200)

The height axis has its own ownership rules — see ADR-013.

---

# The decisions

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

Measured later on Linux (Arch, Hyprland/wlroots, Hyper-V VM with no GPU, so
Mesa's **lavapipe** software Vulkan, which wgpu reports under its Gallium
driver name `llvmpipe` — the adapter is irrelevant to what the *window system*
offers, which is what this table is about):

| Session | Backend | Adapter | `alpha_modes` |
|---|---|---|---|
| Wayland | Vulkan | llvmpipe (LLVM 22.1.8) | `Opaque`, **`PreMultiplied`** |
| X11 (XWayland) | Vulkan | llvmpipe (LLVM 22.1.8) | **`PreMultiplied`**, `Inherit` |

Both Linux sessions reach PATH A, so Linux needs no fallback of its own and
`alpha_mode_for` needs no `Inherit` arm — which was worth measuring rather than
assuming, because the natural guess is that X11 offers only `Inherit`.

**Read this table as a property of the surface, not of the adapter.** The X11
row has no `Opaque` in it: `alpha_probe` creates a *transparent* window, which
on X11 selects a 32-bit ARGB visual, and the capabilities follow the visual.
The same adapter under an opaque window reports `Opaque` normally. That is why
`alpha_mode_for` is asked per surface and why `window.opacity` is fixed at
creation on X11 — the visual is chosen once, when the window is built.

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
managed DirectComposition presenter. It will live behind a `Presenter` trait in
`zest-app`'s platform layer (today a single `platform.rs`) so the renderer never knows
which path is live.
Until it exists, `window.opacity` is honored where the surface reports `PreMultiplied` and
otherwise forced to 1.0 with `Capabilities { transparency: Unsupported(..) }` reported to the
settings layer — **never silently ignored.**

**Related discipline:** window opacity applies **only to cells whose background is
`Color::Default`.** Applying it to every cell double-darkens, and makes cells with an explicit
background (dir colors, TUI panels, `@sigx/terminal-ui` boxes) see-through when they must not
be. Also: opacity never applies to glyphs. Translucent text is unreadable.

---

## ADR-004 — The remote protocol ships grid deltas, not raw PTY bytes

**Status:** accepted (implemented; protocol 3)

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

**Status:** accepted (implemented)

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
clustering requires hosts to advertise dialable addresses, and every away path this project will
ever have is strictly origin-initiated — the tunnel this originally named, and the relay that
replaced it (ADR-009), for the same reason: a laptop behind NAT has no address to advertise. The
host-to-host mount also runs no policies, so it would expose an unauthenticated RPC surface.

Do not "simplify" toward edge actors.

---

## ADR-006 — zesterm is a fleet of hosts, not one machine exposed to the internet

**Status:** accepted (implemented)

The original framing was *"reach **this** machine's shells from the internet"*: one host, many
clients. That is not what the tool is for. Its author works across a Mac, a Windows box and
Linux machines, and the thing worth having is **every machine reachable from every device** — the
Mac's shell in a window on Windows, a Linux build watched from a phone.

So every machine runs a daemon and is a **host**; every window, browser tab and phone is a
**client**; and a machine is routinely both. One client holds sessions from several hosts at
once.

### What this forces, and why it is here on day one

**Sessions are addressed `(HostId, SessionId)` from the first protocol byte.** Retrofitting the
host half means a protocol version bump and a change to every client — desktop, browser, phone —
released separately. The first daemon will only ever serve one host, and the address still carries
both.

**Identities are public keys, not random ids.** "Is this really my Mac" has to be answerable by
asking it to sign a nonce. A UUID is equally unique and proves nothing, and the difference only
shows up once a relay is in the path — by which point every stored id would need reissuing.

**The LAN is a first-class path, not an optimization.** Two machines on one desk are ~0.3ms apart;
routing their keystrokes through Cloudflare and back adds 40–100ms to something that should be
imperceptible. Discovery finds peers locally and connects directly; the tunnel is the fallback for
when you are away. Both carry identical `zest-proto` messages, so identity, authorization and
end-to-end encryption are written once.

### The edge directory, and why it does not contradict ADR-005

A fleet needs an answer to *"which of my machines exist, are they up, how do I reach them?"* — and
that answer cannot live on any single host, because the machine you want to ask about is the one
that is asleep. So there is a small Cloudflare component: **a directory**.

It holds host ids, labels, and last-seen endpoints. It holds **no session state**: no grid, no
scrollback, no command text, no cwd. It is not in the data path — clients connect to hosts
directly, and a directory outage costs discovery, never a running session.

**This is not licence to move sessions to the edge.** ADR-005 stands in full, and its reasoning is
untouched by the fleet: two extra WAN legs per keystroke are just as bad with five hosts as with
one, shell output on someone else's disk is just as unacceptable, and a *local* terminal that stops
working when Cloudflare is unreachable is just as broken. The mesh multiplies the number of hosts;
it does not change where a session lives. If a future change proposes putting session actors at
the edge because "the fleet needs a coordination point", the coordination point is the directory
and it already exists.

---

## ADR-007 — The daemon owns sessions; the GUI app is a client of its own daemon

**Status:** accepted (implemented)

Every machine's terminals are owned by `zest-daemon`. The desktop app attaches to its own local
daemon over a loopback socket — a named pipe on Windows, a unix socket elsewhere — using exactly
the protocol the phone uses over the network.

**Rejected: the app owns local sessions in-process, and the daemon exists only for remote access.**
That is faster by an IPC hop and was still the wrong call:

- **Two session paths drift.** The local path is the one exercised every day; the remote one is
  the one that breaks. Bugs would be found by the client that is hardest to debug.
- **Closing the window would kill the shell.** For a fleet whose point is picking a session up
  somewhere else, a session that cannot outlive its window is the feature negated. Close the lid
  on the laptop, reattach from the phone — that only works if nothing about the session was ever
  the window's.
- **The app would have to grow the remote path anyway**, since a window here must show a shell
  there. Having built it, keeping a second one for local sessions is a choice to maintain two.

### The cost, and where it is actually dangerous

A keystroke now crosses a process boundary: roughly 50–100µs over a loopback socket, against a
10ms budget. Affordable, and worth measuring rather than assuming.

**Startup is where this genuinely threatens something already won.** The window currently paints
at ~43ms because a class background brush lets the OS paint it before any GPU exists, and the
prompt is on the first frame because the shell is spawned *before* GPU initialization so its
~400ms overlaps the driver's ~850ms.

Find-or-spawn-daemon must occupy that same slot — after the window is visible, overlapping the
driver. It must never sit between creating the window and showing it. On the warm path, which is
every launch after the first, it is a pipe open costing microseconds.

`zesterm --startup-probe` prints time-to-first-paint and fails above 100ms, so this is a failing
command rather than a vague sense that it used to feel faster. A flag rather than a `#[test]`
because first paint means a real window on a real compositor, and an assertion that gets silently
skipped in CI protects nothing.

## ADR-008 — E2E ships before the relay, and the relay is a dumb pipe

**Status:** accepted (implemented; protocol 3)

Everything after the `Challenge` is encrypted end to end, on every transport including loopback.
Each side puts an ephemeral X25519 public key into the transcript both were already signing, so
the existing Ed25519 signatures certify those keys and the shared secret is salted with the hash
of the signed bytes.

### Why this comes before the relay, not after

The relay is a Cloudflare Durable Object that both the daemon and the browser dial *out* to,
because neither has a dialable address. That makes Cloudflare a party to every byte of every
session — unless the bytes are already opaque when they arrive. Shipping the relay first and the
crypto second would mean a period during which the honest description of the product is "your
shell, in someone else's process", and would make the encryption a change to a working system
rather than a property it always had.

It also settles a question that would otherwise be argued per-feature: the relay cannot be
trusted with anything, so it is never asked to be. It routes, it counts, it enforces a rate
limit. It never terminates the protocol.

### Signed ephemeral X25519, not Noise

**Rejected: Noise IK**, which an earlier plan named. Two independent implementations of a
*framework* is far more surface than two of one specified handshake; `snow` has no browser twin,
so the browser would get a hand-written Noise anyway; and what Noise buys here — identity hiding
and 0-RTT — is not wanted. The host proving itself first is a *feature* (a client that dialled an
address from an mDNS advertisement must be able to hang up on the wrong machine), and 0-RTT
resumption is meaningless when a redial is always a fresh handshake.

What replaced it is smaller than the thing it replaced: two 32-byte fields in a transcript that
already existed, signed by signatures that already existed.

**Rejected: converting the Ed25519 identity key to X25519** by the birational map. Both dalek and
`@noble` support it, and it would remove the ephemerals entirely. It also reuses one key across
two algorithms, defeats the `Role`/`Purpose` domain separation this codebase invests in
everywhere else, and gives up forward secrecy — a stolen identity key would decrypt every session
ever recorded. The identity key signs; ephemeral keys agree.

### ChaCha20-Poly1305, not AES-GCM

The deciding argument is not the cipher. **`crypto.subtle` is async**, and the web client decodes
and applies deltas on the main thread synchronously — by measurement, not by preference (see
`clients/web/README.md`). AES-GCM through WebCrypto would make the entire browser decode path
async to gain hardware acceleration the client is not bottlenecked on. `@noble/ciphers`' ChaCha is
synchronous.

### The seal switch is positional

The `Challenge` is the last plaintext frame in each direction; the client's `Auth` is the first
sealed one. Not a table of which message types are encrypted — that is a table two independent
implementations can disagree about, and the disagreement is invisible until a frame fails to
open. A position is not.

This has a pleasant consequence: the set of refusals that can be sent in plaintext is exactly "the
host never sent a `Challenge`", which needs no enumerating. And it means a sealed `Auth` that opens
is implicit key confirmation client→host, with the sealed `Welcome` confirming the other way.

The host therefore derives its key *before* it can read the signature that decides whether to
serve. That is not trust granted early: opening the `Auth` proves only that whoever completed the
DH also sent it, and the trust store still decides who gets a shell.

### Loopback is sealed too, and it cost 2µs

ADR-007 rejected two session paths because the local one is exercised daily and the remote one is
the one that breaks. The same argument applies here: an encrypted path that is skipped on the
transport every developer uses is an encrypted path nobody exercises.

Measured rather than assumed — `attach --ping 500`, six runs each, one machine, back to back:
**p50 ≈17µs plaintext, ≈19µs sealed.** Two ChaCha operations per round trip on frames small enough
that per-record setup dominates. Against a 10ms keystroke budget.

### The golden is the artifact that makes two implementations possible

`crates/zest-proto/fixtures/handshake.json` carries fixed seeds, the transcript bytes, their hash,
both directional keys, and sealed records with plaintexts and counters — including two straddling
the ratchet at 2²⁴. Every other fixture pins encoding; this one pins the *key schedule*.

The reason it carries intermediates rather than just sealed records: two implementations can agree
on every transcript field, produce signatures each other verifies, and still derive different
keys. It presents identically every time — the handshake completes and the first frame does not
open — whether the cause was field order, the hash input, an HKDF info string, or a swapped
direction. A golden that only proves the last step names none of them.

It earned this the first time it was used. A throwaway verifier written against it took full HKDF
where the ratchet needs Expand alone — a branch 16 million records away, so it would have shipped
and been reported as "it dies after a few hours".

### The ratchet is deterministic and unannounced

At 2²⁴ records the counter resets, the epoch increments, and the key ratchets. **No message on the
wire.** Both sides count the same records in the same order, so both turn at the same point; a
rekey message would be one more thing two implementations could disagree about, at exactly the
moment a disagreement is undetectable.

### What this supersedes

- `zest-mesh/src/pairing.rs` said the handshake was "entity authentication, not a secure channel"
  and that a future Noise IK closes it. Both were true and are not any more; the module doc now
  describes the channel.
- `zest-daemon/src/ws.rs` deferred TLS on the grounds that the internet path would be a Cloudflare
  Tunnel terminating TLS at the edge. That path is now a relay that is not trusted to terminate
  anything, and LAN `ws://` is no longer unencrypted at the protocol layer.

**TLS is still not here, and is still wanted.** E2E hides the payload; it does not hide that a
connection exists, to whom, or how large its frames are. The relay speaks `wss://` and the
daemon dials it over `zest-cloud`'s `TlsDuplex`.

---

## ADR-009 — The relay is a dial-back pipe, one Durable Object per host

**Status:** accepted (implemented — the relay Worker, dial-back and the desktop attach
leg are live; the remaining hardening items are in ROADMAP.md's open work)

ADR-008 settled *what* the relay is allowed to know: nothing. This settles *how* it is built.

Neither a daemon on café wifi nor a browser on 5G has a dialable address, and mDNS does not cross
routers. Both dial **out** to a Cloudflare Durable Object, which pairs them into a pipe.

### Dial-back, not a mux

The daemon holds one long-lived outbound **control link**. A browser attaching causes a *new*
connection to come into existence through it: the object sends `open`, the daemon dials a second
socket for that pipe alone.

**Rejected: multiplexing session streams over the one trunk.**

1. **Head-of-line blocking is not hypothetical.** One trunk means a `cat` of a 1 MB file and a
   slow browser share one TCP connection and one edge send window — one busy session stalls every
   other session on that host.
2. **A mux reintroduces the exact hazard `ws.rs` was hand-rolled to avoid.** Its module comment:
   two instances over `try_clone`d streams share no write lock, so an auto-queued pong can
   interleave mid-frame with a keyframe. A mux is N logical writers on one socket. Dial-back
   reuses `serve()` unchanged, one connection at a time, which is what `serve()`'s doc comment
   requires.
3. **E2E is per-pipe.** One trunk-level session would make the object an endpoint, defeating the
   thing ADR-008 buys.
4. Hibernation only pays off per-pipe; a trunk is awake whenever any session is.
5. A malformed stream kills one session, not the host.

It pays for itself a third time, in a place that is easy to miss: the handshake watchdog cuts a
connection by `shutdown`ing its socket, and under dial-back **a logical stream *is* a socket**. A
mux would have needed a "cut substream N" control message, a second writer on the trunk, and
precisely the interleaving hazard point 2 rejects.

Cost: one extra round-trip chain on first attach, once. If it ever grates, park one pre-dialled
idle stream at the object.

### One object per host, and where it lands

`idFromName('host:' + hostId)`. Not per user — a five-host user would funnel every stream through
one colo. Not per session — a control link per session is the thing being avoided.

ADR-005's hazard restated: an object "possibly neither near the user nor near their PC,
permanently". **Verified against current Cloudflare behaviour rather than assumed: placement
follows the data centre of the first `get()`, not a hash of the name.** So per-host naming plus
*the daemon connecting at startup* biases the object toward the fixed machine rather than the
roaming client, which is the outcome wanted. `hosts.do_id` stays reserved and unused, and
`get(id, {locationHint})` — which only affects the first `get()` and is best-effort — stays
unspent. Revisit only if a measurement shows a bad colo.

For the same reason there is **no D1 migration**: `hosts` already carries every column, and the
attach ticket's replay set belongs in the object's own storage, never in D1, because it is on the
attach path.

### The relay is a second Worker, and that is not a preference

**Deploying a Worker that owns a Durable Object class evicts every live instance of that class.**
Serving the relay from the same Worker as the web app would therefore drop every terminal in the
fleet every time anyone changed a stylesheet. Two Workers, two `wrangler.jsonc`, two deploy
cadences; the web app is deployed freely, the relay deliberately.

### Nothing may live in instance fields

The object is evicted between messages. **Tags and `serializeAttachment` are what survive**, so
the byte pump derives its pairing from `getWebSockets('pipe:<id>')` every single time. A
`Map<WebSocket, WebSocket>` held in memory is the classic hibernation bug: it passes every test
written against a single live instance and drops sessions in production after the first idle gap.
The guard is a test suite run twice, the second time constructing a new instance before every
handler call — which is what eviction does.

The one legal exception is the promise resolver for a pipe whose host has not dialled back yet,
because a Durable Object cannot be evicted while a `fetch` is in flight. It is legal there and
nowhere else, and it looks exactly like the bug, so it carries a comment saying why.

**And never write storage on the data path.** Keepalive is
`ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('ping','pong'))`, which answers
without waking the object at all — the difference between an idle host costing nothing and costing
a request every thirty seconds forever.

### The arithmetic, corrected

The first draft of this plan put an active session at $0.022/hour and a working day at $5/month.
That was about 20× too high, and the correction matters because the number is what a future
"simplification" of the coalescing floor would be argued against.

Outgoing messages and protocol pings are **free**; incoming bill at **20:1** (a hundred incoming
messages are charged as five requests). One active session at the daemon's ~30ms coalescing floor
is ~8 incoming messages/second ≈ 1,440 billed requests/hour ≈ **$0.0002/hour**. Duration dominates
and is not billed while hibernating; one session for eight hours a day is ~3,500 GB-s/month,
inside the 400,000 GB-s included allowance. A single-user fleet is effectively free.

**The coalescing floor is still load-bearing** — for a better reason than the bill. Unthrottled is
~1000 msg/s: 125× the requests, and, worse, an object that never goes idle long enough to
hibernate, which converts the dominant cost term from zero into continuous. Two policies keep it
there: never write storage on the data path, and relay only when off-LAN.

### The message size limit moved, and chunking would now cost money

An earlier draft called for splitting relay writes into ≤256 KiB messages, against Cloudflare's
1 MiB WebSocket limit. **That limit is 32 MiB as of 2025-10-31**, and `zest_proto`'s `MAX_FRAME`
is 8 MiB and bounds the *plaintext*, so the largest frame this protocol can produce is ~8 MiB plus
a tag — comfortably under. Meanwhile billing is per message, so splitting one 8 MiB scrollback
response into 32 messages multiplies its cost by 32 on exactly the responses the split was meant
to protect.

So frames cross whole. The test survives the reasoning that motivated it: **an 8 MiB `MAX_FRAME`
frame crosses the relay intact**, which is a real assertion about a real ceiling rather than a
rare 3am disconnect discovered later.

### What is rejected, and stays rejected

1. Terminating the crypto at the relay — the entire thing ADR-008 prevents.
2. Authorizing the relay with the session cookie. Two origins make it impossible, and it should
   stay impossible.
3. Letting the attach ticket substitute for pairing. **The relay authorizes transport; the host
   authorizes shells.**
4. Putting the *session list* in a Durable Object or in D1. ADR-006 permits host ids, labels and
   endpoints and nothing else; a session list is cwd and titles — shell context on someone else's
   disk.
5. Running `@sigx/actors`' host in the browser to hold that list. It pulls turns, placement,
   storage, reminders and metrics into a terminal client's bundle to hold one array, and a
   single-tab host is not a control plane, it is a variable.
6. Adding `nodejs_als`/`nodejs_compat` to run that host at the edge instead. That is ADR-005
   undone by a compatibility flag.

### The honest alternative, named so it is not rediscovered

**WebRTC data channels with the object as signalling only** takes the bytes off Cloudflare
entirely. It costs a WebRTC stack in the daemon — large, and against this project's grain — plus
ICE/STUN, plus TURN for symmetric NAT, which is where you pay again anyway. Revisit if the relay
bill ever becomes the reason not to use the product. The corrected arithmetic above says that is
far off.

---

## ADR-010 — Text is sampled per subpixel, because the hinter already assumed it

**Status:** superseded in part by ADR-011. The constraint below is still true and still the
reason the subpixel pipeline exists; the *decision* it reached — subpixel coverage with hinting
off, as one setting — was reversed after measuring against Windows Terminal. Read ADR-011 with
it.

### The constraint, which is not ours

`swash` does not let a caller choose a hinting *target*. It pins one:

```rust
// swash-0.2.10/src/scale/hinting_cache.rs
const HINTING_MODE: HintingMode = HintingMode::Smooth {
    lcd_subpixel: Some(LcdLayout::Horizontal),
    preserve_linear_metrics: true,
};
```

`hint(bool)` is the entire API. So hinting here has only ever meant "grid-fit this outline for a
rasterizer with three times the horizontal resolution" — and until this change we then sampled it
once per pixel. That mismatch does not read as softness. It changes **shapes**: `w` at 13 ppem in
Cascadia Mono came back as three vertical stems and read as `W`, and `o c e C t` lost the baseline
overshoot that `a` kept, so "Close" read a pixel short beside "tab" in one label. (#100.)

**Measured before believing it:** driving skrifa 0.44 directly with
`Target::Smooth { mode: Light }` — DirectWrite's natural mode, the obvious escape — produces a
**byte-identical** bitmap. Cascadia Mono is ClearType-aware, which disables FreeType's
backward-compatibility mode, so there is no gentler target to select. The choice is to match the
sampler to the hinter or to decline the hinter.

### The decision: both, as one setting

**Reversed — see ADR-011.** Recorded as it was argued, because the reasoning is sound and the
conclusion still wrong, which is worth more than a tidy edit.

`appearance.text_antialias` is `subpixel | grayscale`, defaulting to subpixel, and it decides
hinting too — because they are one decision:

- **Subpixel**: `zeno::Format::Subpixel`, and **no hinting**. The two symptoms have different
  axes. Sampling per subpixel is horizontal and fixes the `w`; the flattened overshoot is
  vertical and only not grid-fitting fixes that. Grid-fitting existed to buy horizontal
  crispness that grayscale could not deliver; sampling per channel delivers it directly, so the
  distortion is no longer buying anything.
- **Grayscale**: `Format::Alpha` and hinting on, exactly as before. Unhinted *and* grayscale is
  the blurry corner neither knob wants.

This also closes #84 ("grayscale-only antialiasing reads thin on a 1× Windows display"), which
was the same defect reported from the other side.

### How per-channel coverage is blended

Three coverages per texel cannot go through ADR-003's `One / OneMinusSrcAlpha`: that equation has
one alpha for all three channels. The glyph pipeline emits a second fragment output carrying the
per-channel coverage and blends `One / OneMinusSrc1` — still exactly source-over, and still
**per primitive**, which matters because combining marks are separate instances drawn on top of
their base glyph.

**Rejected: a two-pass blend** (`Zero / OneMinusSrc`, then `One / One`), which needs no device
feature. It is correct for one glyph and wrong for a *range* of them: pass one attenuates the
destination by every glyph in the batch before pass two adds any ink, so wherever two glyphs
overlap the result is additively too bright, into an `Rgba16Float` target with no clamping.
Batching only non-overlapping glyphs would mean sorting by geometry — the same over-engineering
`atlas.rs` already rejects for emoji — and it doubles every glyph draw.

`Features::DUAL_SOURCE_BLENDING` is available on **DX12 (unconditionally, including WARP), Vulkan
where the adapter reports `dualSrcBlend`, and Metal**. Where it is absent, grayscale is used and
said so once in the log.

### Where subpixel is refused, and why that is ADR-003's doing

Per-channel coverage over a **translucent** destination is undefined: the compositor holds one
alpha and cannot divide by three. So `window.opacity < 1.0` forces grayscale regardless of the
setting. On Windows this costs almost nothing, because ADR-003 already found that DX12 reports
only `Opaque` — opacity is forced to 1 there, and the two fallbacks agree rather than fight.

ADR-003 otherwise stands unchanged: output is still premultiplied, and opacity still never
applies to glyphs.

### Consequences worth knowing

- The mask atlas is `Rgba8Unorm` in subpixel mode — **never** `Rgba8UnormSrgb`. Coverage is not
  colour, and `text_gamma`'s stem darkening is defined on raw coverage; an sRGB view would
  linearize in the sampler and silently change what that setting means. Mask VRAM goes from 4 MiB
  to 16 MiB per layer.
- Stem darkening is applied **componentwise**, and the glyph's alpha is `max(cov)` taken *after*
  that transfer. `max` is the smallest scalar that dominates every channel, so `resolve.wgsl`'s
  un-premultiply can never produce `rgb/a > 1` and bloom a fringed edge.
- The mode is a property of an atlas *generation*, not of `GlyphKey`. The key is hashed once per
  cell per frame and there is no scene wanting both modes at once.
- Box drawing stays a one-byte mask in both modes: it is generated geometrically at cell size
  with arms snapped to whole pixels, so it has no sub-pixel detail to carry and subpixel offsets
  would only put a colour fringe on a `│` that is currently one crisp column.
- The dual-source shader is a **separate module**. naga validates `@blend_src` during *type*
  checking, so a module that merely contains the output struct is rejected by
  `create_shader_module` on any device without the feature — one module for everyone would fail
  to start on exactly the machines the fallback exists to serve.

---

## ADR-011 — Grayscale coverage, grid-fitted, and the same for the chrome

**Status:** accepted — settled by looking, against Windows Terminal on the reporter's own screen

ADR-010 reasoned its way to subpixel coverage with hinting off, and shipped it. It was wrong on
both halves, and it is worth being precise about why, because the errors were not careless — they
were measured, just measured badly.

### What Windows Terminal actually does

**It does not use subpixel rendering.** Channel spread on inked pixels in a screenshot of it is
`0.0`; grayscale is its default antialiasing mode. What makes it crisp at a 7px cell is
**grid-fitting**, which puts a one-pixel stem on one pixel instead of spreading it across two.

Measured on the same string at 9pt, against its 11.7% ink coverage and 45.3% of inked pixels
fully saturated:

| | ink | saturated |
|---|---|---|
| subpixel + unhinted (ADR-010's decision) | 16.07% | 23.9% |
| **grayscale + full hinting** | **12.60%** | **43.1%** |

### Why the earlier measurements missed it

Two mistakes, both worth naming because they are easy to repeat:

1. **Hinting was measured at 16px**, where a stem is three pixels wide and grid-fitting has
   almost nothing to do. It moved a blur proxy by 0.5% and was dismissed. At a 7px cell it is the
   whole difference.
2. **It was measured against a broken baseline.** Coverage was still being applied in linear
   space, so every glyph was already too fat and every candidate fix was judged against that. The
   subpixel fringe filter was reverted for "softening the text" on the same basis, and was
   reinstated unchanged once the baseline was fixed.

Fix the known defect first, then evaluate anything else against it. Both errors came from not
doing that.

### The decisions

- **Coverage is linearized before it is used as a weight** (`linearize_coverage`, `common.wgsl`).
  It is a perceptual quantity multiplied into a linear target that the resolve pass sRGB-encodes,
  so 20% coverage emerged at 48% brightness — every edge inflated, every counter filled. This is
  the sRGB transfer and not a tunable.
- **`appearance.text_antialias` and `appearance.text_hinting` are independent**, defaulting to
  `grayscale` and `full`. Welding them made the pair that matches inexpressible.
- **Stem darkening defaults to 2.5**, on light backgrounds and dark alike. Theory says
  dark-on-light needs far less; both were tested and both preferred 2.5, so the per-theme value
  `ThemeEffects` still carries a comment proposing is not needed.
- **The chrome is pinned to grayscale + grid-fitted**, whatever the terminal is set to. The
  settings are the terminal's; the window's own furniture has one right answer.

### The cost, accepted deliberately

Grid-fitting flattens the baseline overshoot on `o c e` while sparing `a`, so a label mixing them
is a pixel inconsistent — which is exactly the "Close reads a pixel short beside tab" half of
#100, now reintroduced in the chrome. It was traded for crispness, which is far more visible: the
unhinted chrome was reported as blurry twice, and nobody has mentioned the pixel since.

`hinting_flattens_the_overshoot_inconsistently` asserts the defect rather than the fix. If
anything ever makes grid-fitting consistent, that test fails and tells us the chrome can have its
overshoot back.

### What did not survive contact

Aggregate metrics. Mid-tone fraction, ink coverage and saturated-pixel share each hid something
that was obvious on the screen — three separate times. The measurements that worked were the
per-stem intensity profile and the per-element peak brightness, both of which look at a few
pixels rather than averaging millions. Prefer them.

## ADR-012 — Chrome tokens are the window's; the ANSI palette is the session's

A theme file carries two halves — the 24 chrome `UiTokens` and the ANSI
palette — and the client-UI handoff's profiles feature (§12) forced the
question of which half a profile may override. The answer shipped across
#162, #167, #171, #176 and #178: **the chrome half belongs to the window
alone; a profile's `color_scheme` reseeds only its sessions' grids.**

### The rule, and its mechanism

One resolved `UiTokens` → one `ChromeColors` per window, converted to
premultiplied linear once per theme change and never per frame. A profile's
scheme resolves to a palette *seed* applied per terminal — ADR-002's own
mechanism, which is why this cost nothing structural: the render path was
already per-terminal (`Viewport { palette: term.palette() }`), scrollback
stores unresolved colors, and a reseed never rewrites history. The one
per-frame fact a scheme contributes (the selection wash) is cached on the
tab's resolved identity, so the redraw path does a field read, not a theme
resolution — resolving there charged an allocation per pane per frame and
made a deleted scheme warn on every caret blink.

Per-tab identity in the chrome is exactly one accent and one glyph
(`tab_color`, `icon`), chosen by `color_from`: the profile's own colour
unless it says the host decides. That is the whole §12 concession, and it is
cheap because it is data on the tab model, not a token resolution.

### Rejected: per-tab `UiTokens`

A tab switch would re-resolve chrome colour and repaint the titlebar, strip
and every panel — per switch, forever. Windows Terminal keeps its app theme
separate from its colour schemes for the same reason, and the handoff cites
it. Nothing in the shipped code path can express a per-tab chrome token,
which is the point: the mistake is now unrepresentable, not merely avoided.

### Decisions that rode in with the feature

- **`profiles.defaults` is a reserved cascade layer** beneath the named
  profile (`user < defaults < named < workspace`), not the root config: it
  must hold profile-only keys the root has no home for, its name is the
  footer's `[profiles.defaults]`, and "every profile falls through to this
  one" is then literally the cascade. The editor's inheritance chips are a
  two-table lookup, honest by construction.
- **Launch-command precedence:** profile `command` > Defaults `command` >
  `""` for a remote host (the far machine picks its own shell — a local
  shell path sent across the wire is the #20 trap's wire variant) or the
  resolved local shell at home. Pinned by test.
- **⌘1–9 stay tab activation.** The design asked for profile launch; the
  chords were shipped, documented muscle-memory. Plain digits launch while
  the launcher menu is open, which honours the design's intent at zero cost
  to fingers that already know the strip.
- **`Mods::SuperShift` is mac-only, enforced.** Ctrl+Shift structurally
  cannot spell a shifted-comma chord (Shift is spent on the modifier), and
  on Windows `super` is the Win key — a chord that fires with an empty label
  is undiscoverable, so off macOS it falls through to the Desktop row's
  shift-blind meaning instead of running something no chip names.
- **App tabs address by reserved sentinels** on the all-zero host: Settings
  `u64::MAX`, Profiles `u64::MAX - 1`. Two parallel work items independently
  picked `u64::MAX`; a test now asserts the pair differs, because the
  collision was not hypothetical — it happened, in review, on the same day.

## ADR-013 — The restater owns the viewport for one repaint, and we own it after

**Status:** accepted (#247) for the *height* axis; extended to the width axis
(#224) once the height model had been paid for in full — the width section is
at the end of this ADR.

ConPTY answers a resize by restating the whole viewport, and its pseudoconsole
buffer is only as tall as that viewport. So a shrink discards what no longer
fits from *its* buffer while our grid keeps it as scrollback, and a grow
repaints the little it kept and blanks the rest. **Our grid holds more of the
session than the restater does**, which is the fact everything here follows
from.

The repaint always has the last word. That bounds *when* the displaced rows can
be given back, not *whether*.

### The rule

The restater owns every visible row from the moment it is told to resize until
its repaint closes. We own the viewport again after that, and the first thing
we do with it is give back the rows the shrink displaced.

Before the repaint the pull is destructive and #200 is right: rows pulled down
are blanked by the repaint, and the pull has already moved them out of
scrollback, so it is history destroyed rather than misplaced. After the repaint
the same pull is free — the tail of the viewport is blank rows the repaint
itself wrote and nothing will write again, and dropping them moves the viewport
window *up* over storage without touching anything the restater said.

### Why this is a byte-stream property and not a timeout

The repaint is bracketed by DECTCEM: the cursor is hidden, the viewport is
restated from home, the cursor is put back and visibility restored to whatever
the inner program had. Settling on the DECTCEM that closes it is independent of
how a read split the stream, which a quiescence heuristic is not: a 200-column
repaint with colour can exceed the 64 KiB parse chunk, and settling mid-repaint
would move the boundary under rows still being written.

**The two halves of a drag are not the same bytes.** `corpus/resize-drag.vtrec`
has both halves of one real drag:

```text
Down:  ESC[?25l  ESC[8;8;100t  ESC[H  <rows, each ESC[K>              ESC[?25h
Up:    ESC[?25l                ESC[H  <rows, each ESC[K>  ESC[8;1H    ESC[?25h
```

ConPTY announces the size (`CSI 8;<rows>;<cols>t`) on the way down and **not on
the way back** — and the way back is the half the settle exists for, so arming
on the announcement shipped a settle that never ran, green throughout because
the test helper emitted an announcement ConPTY does not; `Drag::Down`/`Drag::Up`
is a parameter of that helper now rather than a detail inside it (#271). Arming
is therefore on what both halves share: the cursor hidden and homed before the
first row. The window it opens is narrow by construction — only a grow that is
owed rows sets `pending_restate`, and the alternate screen has no scrollback,
so a full-screen program redrawing the same way cannot arm it.

**A stale repaint is refused by what it did, not by what it said.** A drag
emits resizes faster than ConPTY answers them, so a repaint laid out for a size
the grid has already left is routine — and settling on one pays a grow's debt
against a viewport that has since moved: the blank tail at that moment is
grow-minted and real, so the settle pulls real history into blank rows the
*closing* repaint blanks again, with the pulled rows no longer in scrollback
(#312; `corpus/resize-drag-storm.vtrec` is the gesture recorded for real). An
*announced* repaint names its stale size and is recorded as sat out up front
(recorded, not merely not-armed, because the cursor-home three bytes later
would otherwise re-arm it). An unannounced one has nothing in its bytes to
compare.

What tells a stale repaint apart is its own coverage. A repaint restates the
whole of *its* viewport, every row terminated with `ESC[K`, so one the grid has
outrun stops short of our bottom row. The grid tracks the deepest row the
restater erased since the window opened (`restate_rows_seen`) and the settle
requires it to have reached the bottom; a repaint that did not cover us returns
`false` with `pending_restate` left armed for the one that does — the same
conservation the no-room case practises through the debt.

**A stale repaint laid out *taller* than the grid is refused at its scrolls.**
This section originally ended "a stale repaint laid out larger than the grid
needs no guard at all: its overflow scrolls, and any scroll already cancels
the debt" — which is true and was the bug (#315): that cancel
exists for content moving on, and a stale repaint's overflow is not that. Each
overflow scroll cancelled a debt still owed (stranding the drag) and banked the
scrolled-off row — the repaint's own restatement of content the grid already
held above the viewport — so the host's history carried the same rows twice,
and scrolling up after a drag (or a `cls`) showed them twice. The grid now
keeps a *bracket* flag (`restatement_open`), wider than the armed state:
opened by either arming marker whatever the arming decides — sat-out and
unarmed repaints write rows too, and the shrink phase arms nothing — and
closed by the DECTCEM that closes every repaint. A full-screen scroll inside
the bracket on a restated-elsewhere grid drops the overflowing row instead of
banking it and leaves the debt alone. The residual, accepted and stated: a
program that hides the cursor, homes, and then streams past the bottom without
ever touching DECTCEM would lose those rows from history. Every measured
ConPTY repaint is DECTCEM-bracketed, ordinary output opens no bracket, and a
grid nobody restates never reaches the branch.

Two boundary facts, both measured on the storm capture rather than reasoned to:
**in a storm, only the very first repaint announces itself** — every later one,
shrinks included, arrives bare, so the announced sit-out is the early form of
this decision and never the load-bearing one. And ConPTY **coalesces**: seven
resizes issued within a gesture came back as two repaints, one stale and one
for the final size, so "every resize gets its repaint" is false the moment the
mouse moves faster than the pipe.

**A resize demotes whatever repaint is in flight.** `Session::resize` must
release the terminal lock before telling the pty (the `ClosePseudoConsole`
deadlock), so a resize can land between a repaint's first byte and its last —
and a repaint that was armed when the grid moved was laid out for a viewport
that no longer exists, whatever its coverage ends up being. It is demoted to
sat-out, narrowly: only a repaint *already armed*, never unconditionally, or
the legitimate repaint answering each grow would be sat out too. This also
closes the corner coverage cannot see — the drag landing on the stale repaint's
own size, which then covers every visible row.

`pending_restate` deliberately survives further shrinks: the settle's bounds
(blank tail, rows below the cursor, scrollback held) are its decay, and the
coverage requirement is what makes those bounds sufficient again. Decaying it
on shrink as well would double-count and under-pay a gesture that shrinks
mid-grow and grows again.

**A settle is provisional until something other than a repaint speaks.** The
guards above refuse repaints the grid outran — and at the daemon's actual
cadence (~120ms a step) nothing is ever stale: every step's repaint arrives at
its own size, its settle is legitimate by everything it can see, and rows are
still destroyed, one step at a time. The restater's buffer never got the pulled
rows back, so the *next* step's repaint restates that buffer from home,
overwriting the pull and blanking the tail (`corpus/resize-drag-stepped.vtrec`;
measured live as 23 → 12 non-blank rows across one height-only drag). No local
fact distinguishes an intermediate repaint from the final one — the difference
is whether another resize is coming, which is the future. So the settle does
not try to know it: the pull is recorded (`settled_pull`), and the moment the
next restatement opens — armed, sat out, or arriving after everything was paid
— the grid takes the pull back first: boundary up over the same rows, fresh
blanks minted below (new ids; gaps are fine), the share owed again for that
repaint's own settle to pay out over what *it* wrote. Conservation both ways,
so a gesture of any length pays out exactly once, at its true end. The settle
becomes final when the debt would be cancelled anyway — a scroll, a screen
erase, or a width change (reflow renumbers, and the re-bank arithmetic dies
with the ids) — because the content has moved on and the inverse would eat
rows something real wrote. **A shrink is not on that list, and putting it
there shipped the drag's third leg broken** (#335): after a settle this
viewport holds more of the session than the restater's buffer, *permanently*,
so drag down–up–up and the second shrink's repaint restates the lesser truth
over the fuller screen — and a partial shrink banks nothing itself (the blank
rows below the cursor absorb it), so zeroing the pull there left the re-bank
with nothing to take back and the repaint blanked the pulled rows in place.
The shrink instead *decrements* the pull by exactly what it banks over the
top: those rows are back in scrollback and out of the repaint's reach, the
remainder stays provisional for the incoming bracket, and the two counts
consume each other one for one because a shrink banks from the top of the
viewport, which is exactly where the pull sits.

**The restore is a between-gestures view, and ordinary output ends it** (#341).
An earlier revision of this ADR accepted "output landing between a settle and
a following repaint" as a residual; it was the next reported bug, inspected
live: after a restore, ConPTY's buffer still holds only its kept rows, so the
shell's next render — `ls` typed after the drag — positions with absolute
coordinates in *ConPTY's* row-space, offset from ours by the pull, and writes
land mid-listing with no erase over the tails ("Length Namees",
"AGENTS.mdchain.toml", a block header mid-print). No settle bookkeeping can
absorb that: the divergence is the restore itself, and only a repaint bracket
gives the re-bank a hook. So the first ordinary content op — a print, a
linefeed, a cursor move that is not a restatement's opening hidden home —
**strands** the pull instead: boundary up over the restored rows (into
scrollback, still reachable), blanks minted below, cursor realigned, debts
cancelled, `ViewportRebased` owed to every client. The write then lands
exactly where ConPTY meant it, and the prompt visibly snaps to where Windows
Terminal would have had it all along — which is the accepted trade: the drag
restores the view, and the first keystroke afterwards files it as history.
The opening hidden home is the one cursor move excluded, and its `perform`
arm runs the bracket-open *before* the `goto` for exactly that reason: an
open bracket is what tells the strand to stand down so the re-bank can work.

**Either direction of DECTCEM closes it.** ConPTY restores the inner program's
visibility state, so a full-screen program that keeps its cursor hidden ends the
repaint with `?25l` and never sends `?25h`. Keying off `?25h` alone would leave
the debt unpaid for exactly those sessions and look like the fix simply not
working.

### What bounds the pull

A debt, not the shape of the screen. `restate_debt` is what a restating shrink
actually pushed over the top, so a grow can never invent history; and it is
cancelled by any scroll or whole-screen erase, because the rows are only owed
while they are still the ones immediately above the viewport. Without that,
`clear` followed by a grow pulls history straight back onto the screen the user
just cleared — every row below the cursor is blank, so the shape of the screen
alone cannot tell the two cases apart.

### One flag, two restaters

A *replica* — a grid deltas are applied into rather than parsed into — is in the
same position for the same reason: the keyframe it is about to be handed
restates every visible row. `Grid::viewport_restated_elsewhere` is therefore one
predicate rather than a ConPTY-specific one, set by the transport on a host and
by `Terminal::remote` on a client. A replica never settles: settling runs from
the parser, and nothing may mix the parser with the delta stream.

### Line ids have gaps, and that is not the thing to fix

`truncate_bottom` destroys the newest ids without rewinding the counter. The
shrink path has always done that with the blank rows below the cursor, and the
settle does it with the blanks a grow minted. Rewinding would be the worse
repair: ids are never reused, which is exactly what blocks and clients index on,
and the shrink drops rows that were on screen and may already be named.

So the gaps stay and the arithmetic goes. Both callers that wanted "the oldest
line still held" computed it as `active_row(0).id - scrollback_len`, which is
only right while the numbering is contiguous; across a gap the answer lands
*inside* it, and the host tells every client it may request scrollback from a
line that has never existed. `Grid::oldest_line_id` reads the oldest row
instead. Measured on a grid with an eight-line scrollback: the count said 13,
the oldest row held was 7, and 13 was in the gap.

### The cost

The boundary moving is a change no delta can describe — there is no
`DeltaOp::Resize`, on purpose (`docs/CONTRACTS.md`) — so a settle costs every
subscriber a keyframe (`TermEvent::ViewportRebased`). One per completed drag
rather than one per `ResizeObserver` tick, because only a grow that is owed
something arms it.

### The width axis: anchor on the line the restater still holds

Deferred while the height model was paid for, then decided from its lessons
(#224). Two facts, both measured (`corpus/resize-width.vtrec`): ConPTY restates
*logical lines* and relies on our autowrap, so the two reflows can never
disagree about wrapping — and they disagree about **anchoring**. A narrow is
safe: both sides tail-anchor onto the prompt, row for row. A widen is not: our
reflow puts the prompt back at the bottom, while ConPTY un-wraps its
viewport-tall buffer into fewer rows and restates them **from home**, ELs
below — which lands the erases on real content mid-viewport. Erased in place,
ids intact, nowhere in scrollback: the loss half of what #224 reported as "the
content is fourteen rows too high".

So a width reflow on a restated grid re-anchors the viewport **top-aligned on
the pre-resize viewport-top line** — the line at the top of the restater's
buffer, which we know because we mirror it, and whose post-reflow position the
`Reindex` answers. Rows above it are banked as history
(`Grid::bank_viewport_top` — the strand view again: scroll up to see them);
the blanks minted below are exactly what the repaint's ELs land on, and its
restatement rewrites rows that already hold that content. On a narrow the
anchor line is already at the top and the whole move is a no-op by
construction. The boundary moved, so it costs a keyframe, like every move on
this seam.

One corner carries the whole recording's lesson: when the buffer's top row is
a *fragment* — the continuation of a line that wrapped out of the restater's
buffer — its restatement begins by rewriting that fragment
(`ESC[H crates ESC[K`, verbatim), while our reflow has merged the fragment
back into its whole line. The anchor banks *through* the merged line in that
case, so the fragment lands on a blank; the whole line lives in history and
the fragment row on screen is the restater's honest screen content — Windows
Terminal shows the same one.

---

## ADR-014 — A host publishes its own profiles

**Status:** accepted (#262).

Launch targets on a machine are *that machine's* fact. `zest-daemon` reads its
own `Settings::profiles`, resolves each through its own `profiles.defaults`, and
publishes the result to any client that asks. A `+` launcher three time zones
away lists the Windows box's WSL distros because the Windows box said so.

### The alternative, and why it loses

The obvious cheaper design keeps every profile in the *viewer's* config and pins
each to a host by label — which is what `ProfileMeta::host` already does, and
what shipped in #175. It works, and it does not scale past one person's laptop:
a profile for `wsl.exe -d Ubuntu-24.04` with a starting directory of
`\\wsl$\Ubuntu-24.04\home\andy` is a fact about the Windows box that the Mac has
to be told, by hand, and told again when it changes. Every machine's config
accumulates a stale copy of every other machine's. Design §12 asks for
*"Found on this fleet: 2 WSL distros, 1 SSH host. Generate profiles"*, and
nothing local can answer that question.

**The local pin stays anyway**, and the two are not in competition. A profile
that means "production" — red window, a specific command — belongs to the
*person*, wherever it runs; that is the same argument `color_from` makes for the
tab accent, generalised. So a launcher shows both: a machine's own published
profiles, and the viewer's profiles pinned to that machine, grouped under the
host that will run them. On a name collision the local one wins, because it is
the one the user can edit.

### Resolved on the host, not on the client

The published `command` and `starting_directory` have already fallen through
that host's `profiles.defaults`. The client cannot do this itself — it does not
hold the far config, and a half-specified profile inheriting its command from a
`defaults` table nobody sent would arrive as a row promising nothing.

The corollary is the field that is deliberately **absent**: a `HostProfile`
carries no `host` and no `ask_host`. A profile published by a machine is pinned
to that machine by construction, and re-sending a `host` key would invite a
client to resolve a label against its *own* fleet — the one way this feature
could run a command on the wrong computer. A test asserts neither key reaches
the wire.

### It rides `Sessions`, and that is a cost rather than a fit

`HostMessage` is `#[serde(tag = "t")]`, so a new variant is not additive. That
is the usual reason to avoid one; here it is sharper, and it was read rather
than assumed: `DaemonClient::recv` maps a frame it cannot decode to
`DaemonError::Transport`, which **tears the connection down**. A new daemon
pushing a `HostInfo` variant would therefore disconnect every older app on the
fleet — strictly worse than the `Enroll`/`EnrollResult` precedent, which is
loopback-only and answered by an `Error` the sender can read.

So the offer is an `Option` field on `HostMessage::Sessions`, and `Hello` gained
a `watch_hosts` bool. Less tidy than its own message, and the honest trade:
`Sessions` is already "what this host has to offer you", already both the
`ListSessions` reply and the watch push, and already what a client re-reads on
every reconnect.

**`None` means "nothing new to say"** — not "it has none". It covers a
connection that did not subscribe, a daemon that publishes nothing, a message
with no change behind it, and a peer that predates the field, deliberately, so
a client needs one branch rather than four. The reader is sticky: an ordinary
session push carries no offer, and clearing on one would blank a launcher's rows
every time somebody opened a shell.

### What it cost: `zest-daemon` now depends on `zest-config`

It did not before, and that was a decision rather than an oversight —
`DaemonConfig::shell_integration`'s doc comment records it as *"neither is worth
doing before someone needs the switch"*. Someone does: a machine that cannot
read its own profiles cannot publish them. `cargo xtask check-deps` guards
`zest-core`, not this crate, so the wasm fence is untouched.

The dependency also buys the half that makes this live rather than
connect-time-only: `zest_config::watch` fires on a config edit, `OfferSource`
re-reads, and a generation bump pushes to every subscriber. `OfferSource::set`
drops a reload that changed nothing — not an optimisation, since a file watcher
fires several times per save on every platform, and without it each of those
would put the whole profile list on the wire for every attached client.

---

## ADR-015 — An agent is a client, and only one exit code is unforgeable

**Status:** accepted (#274, #299).

Every AI terminal shipping today is a chat sidebar over a byte stream: the agent
scrapes a pty with regular expressions, guesses when a command finished, and
drowns in progress-bar noise. zesterm needs none of that shape, because the
three things that make it unnecessary already exist for other reasons — a
headless VT emulator producing typed state (ADR-001), semantic command blocks
carrying command, cwd, exit code and timestamps, and a multi-client delta
protocol addressing sessions `(HostId, SessionId)` across machines and sealed
end to end (ADR-004, ADR-006, ADR-008).

So the decision is that **an agent is a client**. `zest-mcp` holds a device key,
attaches, receives the deltas the window receives, and writes
`ClientMessage::Input`. No new data plane, no second VT emulator, no privileged
in-process surface, and no protocol version bump: everything it does is spelled
in messages that already existed.

The consequences are worth stating because each is a thing not built. There is
no agent-facing serialization format, so the grid an agent reads is the
conformance-tested one rather than a second implementation that drifts. There is
no privileged path, so an agent cannot see a session a paired device could not.
And the audit story is the wire's, not a parallel log's.

### The saving is post-VT, and it is a different number from ADR-004's

ADR-004 measures *transport*: ~1 MB of pty bytes against ~3 KB of delta for
`cat 1MB`. The agent-facing number is *model* efficiency and is measured
elsewhere in the stack: a build with a progress bar writes the same row hundreds
of times with `\r`, and the emulator has already collapsed it to one row before
anything an agent reads looks at it. `blocks` carries no output text at all, so
fifty commands of history costs less than one screen of a build log.

### Measured, and the two numbers do not behave alike

`examples/token_probe.rs` runs a command on a real pty and reports the stream,
the deltas, `screen`'s text and `output` per block. The last two come from a
real `Replica` fed the encoder's own output, so they are what a tool returns
rather than a second reading of the host's grid.

`seq 1 200000` — 1.49 MB of pty, roughly 596k tokens of it — reaches a model as
**202 bytes, about 51 tokens**. That is not compression. `screen` answers with
the final grid, so its size is set by the grid and not by how much was printed;
the ratio therefore *improves* the noisier the command is, which no compression
figure does.

The transport number is not a property of the session at all. `zest-proto`
coalesces on **state** — a subscriber holds an encoder shadow and asks for the
difference from what it last sent — so the same run costs:

| deltas asked for | bytes | against the stream |
|---|---|---|
| 1 (an observer that polled once) | 3,254 | 0.2% |
| 223 (a client on a 16 ms frame) | 506,671 | 34% |
| 17,315 (asked after every read) | 11,386,765 | 765% |

The first line reproduces ADR-004's "~3 KB for `cat 1MB`" almost exactly, which
settles what that figure is: **the single-delta floor**, not a saving every
client receives. The last line is larger than the byte stream it replaces —
framing paid thousands of times over on a screen that keeps scrolling.

A `cargo build --workspace` — 338 seconds, 40,306 bytes of pty, ~16k tokens —
reaches a model as **1,667 bytes, about 417 tokens**: the build's tail, which is
what somebody asking "did it build" wants. Its transport cost at the same 16 ms
cadence is 94,618 bytes, *more* than the stream, because a progress bar repaints
a row that a delta must then describe.

So the two claims must not be quoted interchangeably. ADR-004's is about a
protocol and moves with how often you look — and on a chatty, low-volume command
it is a cost rather than a saving. This one is about a *tool result*, is bounded
by the grid, and does not move at all, which is the property an agent surface
needs.

### Two exit codes, and only one of them means anything

This is the part that is cheap to undo by accident, because the two are the same
Rust type and read identically in a payload.

**A block's `exit_code` is the shell's word.** It arrives as OSC 133;D, and
*any program can print those markers* — `cat` a file containing them and the
parser mints a block with a green `exit 0`, structurally unable to tell. That is
not a defect in the parser; a pty is a byte stream and there is nowhere else for
the information to come from.

**`HostMessage::Exited.code` is the process's own status**, read by the daemon
from the child it spawned. Nothing running *inside* the terminal can produce it.

So every exit code this system reports to an agent carries where it came from —
`ExitSource::{ShellMarker, ProcessExit}` — rather than a caveat in a tool
description nobody re-reads at the moment of use. An agent deciding whether a
deploy succeeded needs to know which of the two it is holding, and the
distinction has to survive into the payload because that is the only place it
will still be true.

`run_isolated` exists for this reason as much as for compatibility. It also
happens to be the answer for shells with no integration — fish, `cmd.exe`, and
any shell reached through `ssh` or `tmux`, which injection structurally cannot
touch — but its *primary* property is that its status cannot be forged by the
thing it is running.

### A third provenance class, and it is not the shell's at all

*(Amendment, #491.)* Both exit codes above are facts *about the session's
contents*, and the argument between them is which one a program inside the
terminal could have printed. A block's `author` is not in that argument. The
daemon records it from the authenticated connection that wrote the bytes, so
nothing inside the terminal can influence it — which is why it carries
`daemon_witness` rather than joining `ExitSource`.

The limit belongs in the same breath, because it is the one an agent will
otherwise over-read. OSC 133 decides *when* a block opens. A shell can
therefore mint a block nobody typed, and it will bear whoever wrote last. What
it cannot do is make a block bear a *different* client's id. Provenance, never
authorization — and the apparently stronger design, refusing to open a block
with no recent input, was rejected because a nested integrated shell
legitimately produces one.

The same retention answers a second question the daemon could not previously
ask. `may_approve_devices` is a property of the *transport*, so every loopback
client could answer `PairingDecision` and enrol an arbitrary remote key. An
agent now declines that authority for itself in its `Hello`, and the honest
claim is narrow: the declaration is made at startup, before the agent has read
any terminal text, so an injection that later steers it is steering a
connection that already gave it up. A hostile program that omits the flag is
untouched, and cannot be caught here — on loopback the socket *is* the
authorization. The flag's default is therefore `false`, the permissive answer,
because every already-shipped client omits it.

### The anchor is the tail block, not the next id

`run` writes a command into a shell somebody is already using and then has to
say which block it produced. The obvious rule — record `max(block.id)` before
writing, wait for something above it — **never fires**, and it is worth writing
down here because the code that makes it wrong is three lines in another crate.

OSC 133 `C` reaches `BlockIndex::begin_output`, which mutates `blocks.last_mut()`
in place: it sets `output_line`, `command` and `state`, and mints no id at all.
So the command lands in the *existing* trailing prompt block, at an id at or
below the high-water mark, and only the prompt that follows pushes a new one.
`begin_prompt` then re-anchors that trailing block rather than pushing whenever
it ran nothing (#193), so the id can legitimately never move for a whole
session.

The anchor is therefore the tail block's **identity** before the write, and the
thing to wait for is that block's **state** advancing. The comparison is `>=`
rather than `==` because the two supported shells disagree: zsh emits `C` from
`preexec` and `D` only from `precmd`-when-something-ran, so an empty Enter is a
bare `A` and the id is reused, while pwsh brackets even an empty line and its
next prompt genuinely pushes a fresh id. A rule written and tested on one of
them is wrong on the other, in the direction that fails silently.

**And what says the anchor is gone is the block's presence, not the index's
floor.** `authoritative_from` looks like the field for this and is not:
`erase_screen` lowers it with `min(lowest_gone)`, and a young session's floor is
already 0, so a screen clear that erases the anchor outright moves it by nothing
at all. The block itself is exact, because a new prompt *pushes* rather than
replacing — so an absent anchor never means "the session moved on", only that
something destroyed the rows it described. Read off the floor, `run clear` was
reported as a command that never started and then spent the caller's entire
deadline waiting for it.

Two more consequences that only a live shell showed, both found by driving the
built binary by hand rather than by any test:

**The gap between `D` and the next `A` is a state callers land in.** `run`
returns the instant `D` closes its block; the next prompt arrives a moment
later. Two `run`s back to back therefore hit a session whose tail block has
finished and whose prompt has not been drawn — which must be a *different*
refusal from "a command is already running", because only one of the two is
worth waiting out. The other may not end for an hour, and typing into it puts
the text in that command's stdin.

**An exit can reach a client before the output that preceded it.** The daemon
snapshots `has_exited` before it diffs the grid, precisely so the last screenful
is never reordered past the exit — but the reader thread sets that flag on EOF,
which it can notice while the bytes ahead of it are still queued for the parser.
`exit` typed into a real zsh then arrives as `Exited` first and the `C` that
opened its block a beat later, roughly one run in eight. Nothing on the wire says
"that was the last delta", so a client that stops waiting on the exit needs a
bounded drain afterwards; the alternative is reporting a command that plainly ran
as never having started.

### The fleet comes from two sources, and neither one is the fleet

An agent reaches every machine the window can, and finding out which machines
those are is not a thing the daemon can answer. `Hello.watch_hosts` sounds like
it should — it does not: it carries *this* machine's own `HostOffer`, there is
no `HostMessage::Hosts`, and `zest-daemon` runs no discovery at all. A daemon
knows itself and its sessions.

So the roster is assembled client-side from the two sources `zest-app` uses.
**mDNS** is the local link, and the half it misses is exactly the half the relay
exists for — an enrolled machine on another network. **The account directory**
is the durable half (ADR-006: enrolment is the spine, discovery decorates), and
it knows nothing of a machine on the desk that never enrolled. Either alone
produces a fleet that silently shrinks, which is indistinguishable from machines
being asleep.

What `zest-mcp` must **not** copy is the app's engine. `FleetModel` is threads, a
dirty latch, an `EventLoopProxy` and a poller on a timer, and reproducing it
would make this crate the "second, headless copy of the app's session handling"
its own boundary warns against. Knowing which machines exist is not the
objectionable half; owning a live model of them is. `Fleet::view` is therefore a
*pull*: the multicast socket opens and the control plane is asked on the first
`hosts` call and at no other moment, and a server nobody asks touches neither.
That is also this ADR's own rule from the other side — nothing delivers anything
with no call outstanding.

`best_route` is shared rather than reimplemented (`zest-fleet`, #398), and so is
the relay ladder (`zest_daemon::account::relay_dialer`, #457). A route decision
every surface must agree on, and a sequence where a reused ticket is a security
bug, are the last two things to keep two copies of.

### Listed is not reachable, and the reason is the product

Every machine either source knows is listed, including ones nothing can dial,
each carrying `unreachable_because`. The web client states the rule from the
other side: a machine whose relay is unreachable is still yours, and hiding its
row would make the fleet appear to shrink whenever the network hiccuped — what
that rules out is *the row that must fail*.

For an agent the stakes are different from a greyed-out card. `best_route` is a
three-fact rule and each way of failing it asks for a different act — start a
daemon, join a network, sign in — so "unreachable" is a dead end where "it is
one of the account's machines, but this server is signed out" is a next step. A
listing and a refusal produce that sentence from the same function, so they
cannot disagree about which fact was missing.

### The first dial to a new machine must not hang up, and that is not obvious

Loopback does not consult the trust store — `auth.rs` argues a check there would
be theatre, since a process that can open the socket can already read the key it
would check. A **remote** host's `Auth::Proof` genuinely gates, so the first dial
meets a person comparing six digits.

The obvious arrangement is to refuse the tool call at once, let them approve, and
retry. **It can never succeed.** `PendingHandle::Drop` cancels the request — "a
prompt for a device that has already hung up is exactly what teaches someone to
dismiss prompts without reading them" — so hanging up deletes the very prompt it
is asking them to answer, and the retry mints a fresh code they must be told
about again. The failure looks like a person being slow.

So the dial keeps running on a thread of its own, holding the request alive,
while the call returns the code immediately; a later call collects the
connection. One dial in flight per host, because the queue resolves by
`ClientId` and a second would raise a second dialog that the first approval
answers anyway.

This is what makes the durable key load-bearing rather than tidy. `agent-key` is
a **third** principal beside the daemon's `host-key` and the window's
`client-key`: the approval writes *that* key into the far machine's trust store,
so every later launch authenticates outright, and `zest-daemon --forget` revokes
the agent without revoking anyone's laptop. It is read from the OS keychain on
the first remote dial and never at startup — on macOS an ad-hoc-signed dev build
loses its Keychain grant every `cargo build`, and a tool server that blocks on a
modal prompt before it can answer `initialize` is a broken tool server.

### This client's keystrokes are encoded server-side, and that is an exception

`crates/zest-input/src/lib.rs` states the rule the other way round: the protocol
has the *keyboard* encode a keystroke, because modifier conventions belong to the
platform that produced the event, and every Rust consumer already holds a
`winit::KeyEvent`. An agent holds nothing. It has no keyboard, no layout and no
platform, so `zest-mcp` takes named keys — `down`, `shift+tab`, `ctrl+c` — and
encodes them from the session's own `Modes`.

That is forced rather than chosen. An arrow is `ESC [ A` or `ESC O A` depending on
DECCKM, which is set by the program and lives on the host, so the encoding cannot
be done anywhere else and be right. Asking a model to emit the sequence itself was
measured at roughly 2 attempts in 10 reaching the application (#345). The cost is a
third implementation of one table, and it is paid down by a test rather than by
care: `zest-mcp/tests/keys.rs` holds it byte-for-byte against `zest-input`.

### The paste boundary has to be stated, because it cannot be inferred

A terminal distinguishes typing from pasting, and applications act on the
difference — that is what DEC 2004 is for. Two things follow for an agent surface.

**A write boundary is not a read boundary.** Sending text and its Enter as two
`ClientMessage::Input`s gives two `write`s on the pty and nothing more: a tty
hands the next raw-mode `read()` everything queued, and on Windows conhost parses
the input pipe into console records on its own schedule. So splitting removes the
case that was always wrong and leaves a race. The boundary that survives is one
carried *in the byte stream* — the bracketed-paste markers.

**And which of the two was meant cannot be guessed from the mode.** 2004 is set
for a program's whole run, not for the moments a paste would be right; `nvim` has
it on in normal mode. So wrapping text automatically whenever the bit is set would
turn `:wq` into a buffer insertion, silently — a wrong action that looks like
success, which is the failure class this crate spends the most effort avoiding.
`input` therefore has `text` and `paste` as separate arguments, mirroring the web
client's `text.ts` and `paste.ts`, and infers nothing. (#344)

### The trap this ADR was written after

`HostMessage::Exited { code: Option<i32> }` existed on the wire from protocol 2
and its sole producer hard-coded `code: None` until #299. The field was decoded
by every client and filled by nobody, which is indistinguishable from a host
that genuinely could not determine a status — so nothing looked wrong, no test
failed, and the one trustworthy exit code in the system did not exist while the
roadmap described it as the reason a tool was worth building.

`zest-pty` had `wait_for_child` on both platforms the whole time. The gap was
never the hard part; it was that nothing joined them, and a wire field's default
value is not a place anybody looks. **A field nothing fills reads exactly like a
field nothing can fill.**

### Rejected

**A streaming or polling tool** — "watch this session and react". It is the one
addition that turns prompt injection from *needs the agent to be steered* into
*fires on its own*, and its absence is a mitigation rather than an omission.

**Amended (#319): the line is "no call outstanding", not "no waiting".** As
first written this rejected *polling* as well, which reads as forbidding
`screen(after_seq:)` — a read that blocks until the screen moves past a
sequence the caller names. That is the wrong place for the line, and drawing it
there had a cost: an agent supervising a build slept and re-read a whole screen,
mostly unchanged, which is worse on every axis *including this one*. More
attacker-controlled output crosses into the model per unit of progress watched,
and every re-read is another chance for something in it to be obeyed.

What makes "watch and react" dangerous is not that the agent waits. It is that
output arrives with **no call outstanding**, so the delivery itself manufactures
a turn — and a turn nobody asked for is one that injected text can steer. A
blocking read cannot do that. The agent called it, the answer is that call's
result, and nothing runs afterwards unless the harness grants another turn. It
is `screen` with a deadline, not a subscription: no callback is registered,
nothing is pushed to a process that is not already blocked on a reply, and the
wait is bounded by the same ceiling rule as `max_lines` and `timeout_ms` — the
caller supplies a deadline and `MAX_TIMEOUT` decides what it is worth.

So the rejection stands, restated as the property rather than as the mechanism:
**nothing delivers output to the agent without a call outstanding.** A tool that
returns when something happens is allowed; a tool that speaks when nothing asked
is not, however it is spelled.

**An agent loop of our own.** Harnesses exist and improve monthly; a terminal
shipping an inferior one ages badly. Be the substrate.

**Trusting the caller's bounds.** `max_lines` and `timeout_ms` are clamped,
because the caller is a model and an argument that can lift a ceiling is not a
ceiling. Zero means zero for both — reading it as "unbounded" is the reading
that once let `max_lines: 0` switch truncation off entirely.

**Amended (#63): redaction belongs in core, and the prompt-boundary filter is
rejected before anyone builds it.** Consent and redaction are still open work
(roadmap § Agents), but their placement was argued when this workstream was
first staged and is worth recording before the obvious version gets built: a
filter on the way out, where `zest-mcp` assembles a tool result. That shape
protects exactly one consumer — the moment two paths reach the same session,
one of them is unfiltered, and many paths reaching the same session is what
this system *is*: the window, the phone, the browser and the agent all read
the same deltas. A `Redactor` over the grid in `zest-core` masks the *delta*,
so every client sees one masked truth — the same shape of argument that keeps
the live palette in core (ADR-002) and the block index host-side rather than
in each reader. The unit of consent is the `Block`: the largest thing a person
can actually read before deciding, and the smallest thing that is meaningful
on its own.

And redaction is wanted for a reason ADR-004 must not be read as covering.
"Clients never interpret VT" structurally eliminates escape-sequence injection
into client *parsers*; it says nothing about a consumer that reads the text
**for meaning** — a build log can carry instructions addressed to a model.
The no-call-outstanding rule above keeps such text from manufacturing a turn;
redaction in core bounds what a steered agent can be made to leak, because an
agent cannot exfiltrate what no client was ever shown. Two mitigations, aimed
at the two halves of one hazard.

---

## ADR-016 — Predicted echo is an overlay, and the client still parses no VT

**Status:** accepted (#442). Landed in three parts: the engine and its
cross-port fixture, the native overlay, the browser overlay.

Over the relay a keystroke round-trips 60–120 ms before its echo comes back as
a delta, and that is the whole felt difference between a local shell and a
remote one. mosh's answer — guess that a printable lands as itself at the
cursor, draw the guess provisionally, take it back when the server disagrees —
is the largest perceived-latency win available, and ADR-004 already describes
this protocol as mosh's state synchronization with a block layer on top. This
ADR is the other half, and three decisions about where it may not go.

### A prediction never enters the grid

`Terminal::remote()` is the one door for writing a replica without parsing VT,
and its rule is that a delta stream and any other writer are two authorities
over one grid, the loser being whichever wrote last. A predictor writing cells
is exactly that second writer. It is also a *sharing* problem before it is a
consistency one: the replica grid is what the block index, the selection,
`zest-mcp screen` and every other attached device read. A guess made by the
keyboard in front of one person must not become a character an agent acts on
or a line in someone else's scrollback.

The IME preedit had already settled this (`Viewport.preedit`, "not in the
grid, deliberately"): provisional text is an **overlay** the renderer draws on
top of cells. Predictions ride the same seam in both clients — `Viewport` in
`zest-render-wgpu`, a `predicted` span on the DOM prompt row in the browser
(the canvas painter needs nothing: the alternate screen is never guessed
into) — so nothing that reads the grid can ever see one. That is not a nicety; it is what keeps ADR-015's
"an agent never reads a guess" true without a single line of code in `zest-mcp`.

### The client still interprets no VT

ADR-004's "two emulators means two truths" is a structural property, and the
predictor does not dent it: it handles printable characters and a Backspace
over its *own* guesses, and nothing else. Enter, arrows, control characters and
chords flush the pending guesses, because what they do is the shell's business.
The alternate screen is never predicted into — a full-screen program decides
for itself what a key does — and neither is the cell past the right edge,
because where the next glyph goes is the shell's wrapping rule, not ours. No
escape sequence is ever read client-side, so a client still never parses
attacker-authored bytes.

### Reconciliation reads the delta, not a new wire field — for now

Nothing on the wire says "this delta reflects input up to N". mosh has that
(the server acknowledges input sequence numbers); protocol 3 carries
`Input { session, bytes }` and nothing more. The engine (`zest-proto::predict`,
ported rule for rule to `clients/web/packages/proto/src/predict.ts`) reconciles
from what a delta already carries: a `Row` op is the whole row, a `Cursor` op
is where the host has got to, and the caller supplies a clock.

- A guess is **confirmed** when the host's cursor has passed its cell and the
  row delivered in the same delta holds the character — or, with no row
  delivered, when the cursor alone has passed it, the row having ridden a
  state the client already held. A confirmation is also one latency sample.
- A guess is **refuted** when a delivered row holds something else where the
  cursor has passed, or when it outlives three measured round trips. One
  refutation flushes everything and goes *quiet*: guesses are still tracked,
  so the link is still measured, but none is shown until one confirms. This
  is the `Password:` prompt — a line that is not echoing stays that way, and
  the next line proves itself by echoing.
- A row the cursor has **not** reached says nothing. The host may simply not
  have processed that key yet — typing `ab` fast lands `a`'s echo with `b`
  still in flight — and nothing on the wire distinguishes "not yet" from
  "never". That ambiguity is the exact thing an additive `echo` sequence on
  `Input` and `Update` would close. It is deliberately not built yet: the
  additive rule allows it, but it touches the daemon, both clients, the
  bindings and the fixtures in one PR, and the heuristic has to be seen
  failing on a real link before that is paid for.

### Shown only where it helps

A dim glyph a millisecond ahead of the real one is a flicker, not a feature,
so the overlay is drawn only once the measured press-to-echo latency exceeds
40 ms (hysteresis down to 20 ms). Before any measurement exists the caller's
hint decides — a relayed host is worth predicting on sight, loopback never is.
The policy is three-valued (`auto`, `always`, `off`) and lives in the shared
engine, so the two clients cannot disagree about it.

### Why one fixture and two ports

The three keyframe take-back rules drifted into three semantics because each
reader had its own (#313). The predictor has two readers by construction and
one rule set by construction: `fixtures/predict.json` is hand-authored, one
scenario per rule above, and both `tests/predict.rs` and `predict.test.ts`
replay it step for step. A rule that changes changes the fixture, and a port
that disagrees fails by name.

---

## ADR-017 — A background picture *is* the window background, not a layer over it

**Status:** accepted (#450). The pipeline half of #144, which specified the
per-profile field and could not have it.

The renderer had SDF rects, glyphs and decorations, and the only textures it
could sample were the glyph atlas's mask and colour layers. A background
picture therefore needed a fourth pipeline, and the interesting question was
never how to sample a texture — it was where the picture sits relative to the
three things that already paint a pane's background.

### It replaces the window background; it does not blend over it

The offscreen is cleared to `Scene::backdrop`, which carries `window.opacity`
premultiplied. Drawn with the usual `One / OneMinusSrcAlpha`, a quad whose own
alpha is also the window opacity composites against that clear to
`1-(1-o)²` — the pane comes out visibly *less* transparent than the padding
around it. This is not a new hazard: `Scene::push_window_background` already
skips its rect whenever the clear is the same colour, and its comment does the
arithmetic. The picture is the same layer, so it inherits the same rule.

So the image pipeline is the only one built with **`blend: None`**, and its
fragment stage emits the finished pixel:

```
rgb   = mix(base.rgb, texel.rgb * base.a, texel.a * (1 - dim) * inside)
alpha = base.a
```

`base` is the window background the pane would otherwise have been filled
with, carried per instance rather than read from the clear — a split pane on
its own palette, or a session that ran `OSC 11`, does not have the clear's
colour. `inside` is zero outside the picture's placement, which is what makes a
Fit letterbox and a Watermark's margins come out as plain background rather
than as black.

The property this buys is worth stating as an invariant, because it is what
the tests assert and what a future change would break silently:
**at `dim = 1` the output is byte-identical to the rect it replaced.**
`dimming_all_the_way_is_the_plain_background` compares the two through the
same resolve pass, and `a_picture_composites_the_window_opacity_exactly_once`
pins the alpha at 0.8 rather than 0.96.

### The cells need no new rule at all

ADR-003's discipline — window opacity applies only to cells whose background is
`Color::Default` — turns out to be exactly the discriminator a picture wants.
`emit_row_backgrounds` already skips any run whose resolved fill equals the
window background, so a default cell emits **no rect**, and the picture below
shows through it. A cell with an explicit background emits one and hides the
picture, which is what keeps `ls` colours, TUI panels and `@sigx/terminal-ui`
boxes readable over a photograph. Not one line of `cell_bg` changed.

### Per viewport, never under the chrome

ADR-012 settled that a profile's scheme applies to its grid only. A picture is
the same kind of thing, so it rides `Viewport` beside `opacity` and is clipped
to the pane: two panes running two profiles can carry two different pictures,
and the tab strip stays in the window's theme. A window-level picture behind
the chrome would have been less code and is the wrong shape — it cannot be
per-profile, which is where §12 puts the field.

### The decoder lives in `zest-app`

`zest-render-wgpu` takes RGBA8 bytes and a size and nothing else, so it gains
no dependency on the image-format zoo; `image` is already linked into
`zest-app` for `--screenshot`. Two details ride along:

- The texture is **`Rgba8UnormSrgb`**, so the sampler linearizes for free. This
  is the opposite of the atlas's masks, which ADR-010 insists are *never* sRGB
  views — and the two are consistent, not in tension: a photograph is colour,
  a mask is coverage, and only one of them has been through a transfer
  function.
- Pictures are capped at 4096 per axis. A phone camera's 6000×4000 is 96 MB of
  RGBA8 sampled onto a pane a fraction of that size, and the cache is keyed by
  path with a generation bump on config reload, so experimenting with five
  wallpapers does not leave all five resident.

**A picture that cannot be loaded draws nothing** — not a black pane, not a
dialog. The settings row is a text field, so every prefix of a path someone is
typing is a file that does not exist, and the only honest behaviour is for the
window to look exactly as it did before they started.

## ADR-018 — One process, many windows: `App` is the window, `Process` is the process

**Status:** accepted (#490; the epic is #489).

For as long as there was one window, `App` was the whole application: it
implemented winit's `ApplicationHandler`, held the one `Window`, the one GPU
surface, the one tab strip, and `window_event` threw its `WindowId` away.
Closing it was `el.exit()`. A second window meant a second process — which
fought the first over one `tabs.json`, started a second mDNS browser, and on
macOS asked the Keychain a second time.

### The split goes the cheap way round

Nearly every field on `App` is genuinely the window's — its surface, its
strip, its overlays, its pointer, its animation springs. The alternative that
sounds cleaner, a new `Window` struct that those ~120 fields move *into*, is
fifteen thousand lines of `self.x` becoming `win.x` for no behavioural gain.
So `App` stayed the window, and the handful of things that are one per
process moved **out** into `process::Shared`, reached through an `Rc`:

| On `Shared` | Because two windows each having their own would… |
|---|---|
| `next_placeholder` | mint the same placeholder address twice, and a placeholder address is what routes `TabExited` / `SessionGone` / `Attention` to a window — a tab's exit would close a tab in the wrong window |
| `fleet` | run two mDNS browsers and two probers |
| `approval` | show the pairing modal in one window and not the other, for a question that is about the machine |
| `clipboard` | on X11, take the copied text with the window that set it |
| `remote_identity` | prompt the keychain once per window |
| `local_context` | build two context engines for one machine's in-process sessions |
| `restart_pending` | let one window's settings tab forget a restart another window's owed |
| `activity` | stop the sidebar's age column for a tab moved to another window (#501): the tab's wake callback keeps the map it was built with, so that map must be the one every window reads — it is per-session data anyway |

`config` and `settings` are deliberately **per-window clones**: a reload
lands as a broadcast and each window re-derives its own `Config`, which is
also what ADR-012's "one `ChromeColors` per window" needs. The account state
machine stays per-window too; it is driven by a Fleet screen inside a window,
and hoisting it would put ~44 sites behind `RefCell` borrows across the
longest methods in the crate for a limitation that costs nothing today.

The rule for the next field someone adds to `App`: *would two windows break
by each having their own?* If yes, it goes on `Shared`. If the answer is
merely "it would be duplicated", it stays.

### A window asks; the process decides

`Process` implements `ApplicationHandler` and owns `Vec<App>`. It routes
each `window_event` by id, and each `Wakeup` by a pure, exhaustive
`process::route`: a wakeup that names a session goes to the window whose
strip holds it (`TabStrip::owns`, tabs *and* panes), the fleet latch is
consumed once by the process — `take_changed` clears one flag, so a broadcast
would starve every window but the first — and everything else is broadcast,
because each of those arms was already a no-op for a window it does not
concern.

`App` never calls `el.exit()` and never opens a window. It records intent in
`WindowRequests { close, new_window, persist, tear_off }` and the process
drains that after every dispatch. A tear-off (#501) is the same shape one
step further: the process takes the `Tab` out of one window whole — its
session, its connection, its wake callback — and opens a window around it
(`FirstTab::Adopt`), which sizes the pty against what the tab was *last
told* rather than against the new grid's defaults; the source window,
emptied, asks to close in the same pass. Address routing makes the moved
tab's wakeups follow it with no further work. That is what makes "close this window" and "quit"
different things: the process drops the closing `App` — every tab's session
detaches in its destructor, exactly as before — and exits the loop only when
no window is left. The probes and `--screenshot` are `FirstOnly` flags handed
to the first window alone; they measure or photograph *a* window and leave
from inside it, which is why `Process::resumed` checks `el.exiting()` after
each open.

### The file remembers windows, and closing one is a decision about it

`windows.json` replaces `tabs.json`: every window's tabs plus its geometry,
in physical pixels because that is what winit reports and takes back. The
old file is read as one window with no geometry until the first successful
save of the new one, then removed. A saved size outranks `window.columns` /
`window.rows` — a person resized *that* window; the setting describes windows
with no memory yet. A saved position is kept only where at least 64×64 px of
the window would still land on some monitor (`windows_state::place`), because
a window restored off-screen is a window the user cannot find.

Closing one window of several **forgets it**; closing the last remembers it.
The first is what the user asked for, the second is quitting, and the single
window always came back after quitting. `snapshot_after_close` is the whole
rule, and it is tested.

### What this deliberately does not do yet

- **Share the GPU device.** Each window has its own `Gpu` — instance,
  adapter, device, pipelines, atlas. `Renderer` already takes the device by
  reference, so sharing is a mechanical split of `init_gpu`; it lands when a
  measured second-window cost says it should, not before.
- **Address `Redraw`.** `wake_for` stamps a session address on `Attention`
  and could on `Redraw`; today `Redraw` is broadcast and each non-owner's
  `RedrawRequested` finds nothing dirty and skips. The follow-up is one line
  if profiling ever asks for it.
- **Stay alive with no windows on macOS.** winit 0.30 exposes no
  `applicationShouldHandleReopen`; closing the last window quits on every
  platform.

### A second launch is a request to the running process (#497)

`zesterm` run while one is running does not become a second process; it
asks the first to open what it was given. The rendezvous is a second per-user
local endpoint beside the daemon's — `zesterm-app`, a unix socket or a named
pipe — built on the daemon's own transport (`zest_daemon::LocalListener`,
*lifted* out of `listen` rather than copied, because the flock-unlink-bind
sequence and the overlapped `ConnectNamedPipe` are the paid-for traps and a
copy is how one of them loses a step). It is deliberately **not** the daemon
socket: the daemon is a session server, and "open a window" is nothing a
session server should answer. Not loopback TCP either, for `local.rs`'s
reason — the socket mode or pipe DACL is the authorization.

Three rules hold it together. **A launch never hangs**: the launcher waits
500 ms and then opens its own window, because a window too many is
recoverable and a launch that does nothing is not. **The instance answers
`Ok` only after the window exists**, and its acceptor's budget is *shorter*
than the launcher's, so a wedged loop produces no answer rather than a late
`Ok` to a launcher that has already left. **A different build is a different
program**: the greeting carries the binary's own length and mtime, so
`zesterm-dev`'s rebuilt binary never forwards to the stale one it replaces —
one `stat`, no `build.rs`, and it distinguishes two builds of the same dirty
tree where a git sha would not.

The claim (one flock, or one `CreateNamedPipeW`) happens in `main` before
the window, never between creating and showing it, and serving starts after
the first paint — ADR-007's budget, kept. What forwards is what a running
process can take: `-e`, `--profile` (as a launch, not as a cascade layer),
`--screen`, `--attach`, and the directory the launcher was run from. A process
whose flags are a config layer of its own (`--theme`, `--size`, …) neither
forwards nor serves — a window opened later on someone else's behalf must
not carry it — and the probes and `--screenshot` never touch the endpoint.
`window.launch = window | tab | instance` is the setting; the values are the
flag suffixes so the override needs no table.


## ADR-019 — A paired device may edit a host's config; the gate is the pairing, not the transport

**Status:** accepted (#498, #509).

The config surface (`GetConfig`/`ConfigState`, `SetConfig`/`ConfigWritten`) lets any
paired client read and change a machine's settings, profiles and theme choice —
over the LAN and through the relay, not only from loopback. That is a security
posture, and it runs against the shape of every other *authority* in the daemon:
`PairingDecision`, `Enroll` and `watch_pairings` are all loopback-only, on the
argument that joining a machine to an account is the authority of whoever is
sitting at it. This one is not, and the reason has to be written down, because
"make it loopback-only like the others" is the obvious review comment and it is
the wrong call.

### It is not a new privilege, and that is measured rather than assumed

`resolve_for_write` (`crates/zest-daemon/src/files.rs`) joins, canonicalizes and
follows symlinks. **There is no path allowlist of any kind** — not to a repo, not
to a cwd, not to anything. So `ClientMessage::WriteFile`, which shipped for the
editor (#446), already lets a paired device rewrite
`~/.config/zesterm/config.toml` byte for byte. `CreateSession { command }` already
runs an arbitrary command as the daemon's user. A paired device is, today, in
full control of the machine it is paired to.

Refusing `SetConfig` over anything but loopback would therefore withhold no
capability. It would withhold only the **validation**, and push a client that
wants to change a setting back to writing the file blind — which is strictly
worse, for a reason specific to this file:

`cascade::resolve` ends in `try_into::<Settings>().unwrap_or_default()`. A single
wrongly-typed value — `typography.size_pt = "big"` — resets the **entire settings
tree** to defaults in every running client. Themes, fonts, padding, tab layout,
all of it, silently, with nothing anywhere naming the cause. `zest-daemon`'s
`config::check` applies the edit in memory and refuses if the result no longer
deserializes. That check is the whole value this message pair adds over the status
quo, and a gate that routes around it buys a feeling of safety by removing the
only real safety there is.

### Three things that *are* different from `CreateSession`, and what answers them

Naming them is the point; each is already true of `WriteFile`, so none is
introduced here, but a reader deserves them stated rather than waved past.

**Persistence.** A session dies with the process. A written `shell.command` runs on
every tab the *human* opens from then on, outliving the paired device's access —
revoking a pairing does not revoke a config edit. Nothing structural answers this;
it is the honest cost of the file being editable at all, and it argues for the
audit line below rather than for a gate.

**Reach.** An agent's session is the agent's. A config edit changes the person's
own windows.

**Invisibility.** This is the one worth engineering against. A session appears in
`sessions`; a file write appears in a diff; a settings key that quietly moved
appears *nowhere*. So every `SetConfig` is logged at `info` with the remote, the
op, the key, the resolved path and the **outcome** — a refused write and a
completed one must not read alike in a log — on `WriteFile`'s own precedent.

### What is refused anyway, and why those are different

Two things are declined regardless of who asks, because neither is about trust:

- **A workspace `.zesterm.toml` is never the target.** `trust::is_trusted` gates
  workspace configs precisely *because* they can set `shell.command`, and a wire
  write cannot add the trust entry — that is a person's decision at the machine.
  A written-but-untrusted workspace file is one that is silently never loaded: a
  write that reports success and does nothing, which is the worst outcome
  available.
- **`profiles.defaults` is not a profile.** It is the layer every profile falls
  through, and creating or renaming it would put a launch target in the launcher
  that starts nothing.

`--no-config-writes` exists for a host that wants to answer questions and refuse
edits. It is a **daemon flag and never a settings key**, for `shell_integration`'s
reason sharpened: a config key authorizing config writes is a key the first write
can flip.

### What this extends

ADR-015 says an agent is a client, and that it "cannot see a session a paired
device could not". The config surface is that sentence applied to a second
resource, with one clause added that ADR-015 did not have to make: **an agent
cannot change anything a paired device could not — and persistence is the one
place where "could not" is doing less work than it looks like it is.** A
capability that outlives the access that used it is worth a log line even when it
is not worth a gate.

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
| `crates/zest-daemon` | Session ownership and lifecycle; loopback, LAN, WebSocket and relay transports |
| `crates/zest-mesh` | Ed25519 identity, keystore, mDNS discovery, pairing, trust store |
| `crates/zest-cloud` | The one crate that owns rustls and HTTP: `TlsDuplex`, enrolment, relay dialling |
| `crates/zest-mcp` | Terminals as an agent's tools, over MCP on stdio (ADR-015) |
| `xtask/` | The gates: `check-deps` plus schema / bindings / fixtures / web-export generation and checks |
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

**Status:** accepted (#247), for the *height* axis. The width axis is #224 and
is deliberately not decided here.

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
erase, a width change or a shrink — because the content has moved on and the
inverse would eat rows something real wrote. The residual: output that neither
scrolls nor erases, landing between a settle and a following repaint, would be
re-banked with the rows it overwrote — accepted, because mid-gesture the shell
is not speaking, and a shell that does speak almost always scrolls or erases.

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
happens to be the answer for shells with no integration — `Shell::detect`
returns `None` for `/bin/bash`, with a test pinning it, so "no blocks" is most
Linux hosts rather than an edge case — but its *primary* property is that its
status cannot be forged by the thing it is running.

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

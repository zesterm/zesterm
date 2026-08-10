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

---

## ADR-006 — zesterm is a fleet of hosts, not one machine exposed to the internet

**Status:** accepted (design; implementation is M3–M4)

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

**Status:** accepted (design; implementation is M3)

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

**Rejected: Noise IK**, which the roadmap named at M5. Two independent implementations of a
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
  and that "M5's Noise IK closes it". Both were true and are not any more; the module doc now
  describes the channel.
- `zest-daemon/src/ws.rs` deferred TLS on the grounds that the internet path would be a Cloudflare
  Tunnel terminating TLS at the edge. That path is now a relay that is not trusted to terminate
  anything, and LAN `ws://` is no longer unencrypted at the protocol layer.

**TLS is still not here, and is still wanted.** E2E hides the payload; it does not hide that a
connection exists, to whom, or how large its frames are. The relay needs `wss://` for the browser
regardless, and the daemon will need a TLS client to dial it — see the roadmap's M5/M6 rows.

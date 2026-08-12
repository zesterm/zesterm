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
clustering requires hosts to advertise dialable addresses, and every away path this project will
ever have is strictly origin-initiated — the tunnel this originally named, and the relay that
replaced it (ADR-009), for the same reason: a laptop behind NAT has no address to advertise. The
host-to-host mount also runs no policies, so it would expose an unauthenticated RPC surface.

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

---

## ADR-009 — The relay is a dial-back pipe, one Durable Object per host

**Status:** accepted (design; implementation is M6)

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

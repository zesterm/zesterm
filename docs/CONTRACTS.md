# Frozen contracts

These seams were frozen because several workstreams were building against them at once, and a
seam that moves under three streams costs more than the change was worth. **That reason has
expired — one lead, one lane — and the freeze has not.**

What it protects now is different and longer-lived: a wire type is consumed by clients that are
not in this repository and do not ship on this repository's schedule. Today those are the web
client and the phone, and neither exists yet, which is exactly why now is the cheap moment for
anything that has to change at all.

**The rule, restated for one lead:** a frozen contract does not move casually, and never moves
half-way. To change one — land it with **every** consumer in the same commit, update the table
below, and say so on issue #1. A frozen contract with a half-updated consumer is worse than
either shape.

Adding a *new* type next to a frozen one is always fine. Adding a `#[serde(default)]` field is
fine. Changing a signature, renaming a variant, or removing a method is a deliberate act with a
paragraph of justification attached.

---

## The seams

| Contract | Where | Status | Consumed by |
|---|---|---|---|
| `PtyTransport` | `zest-pty/src/lib.rs` | **frozen** — `hangup` added, see below | WS-C, WS-D, WS-F |
| `HostId`, `ClientId`, `SessionId`, `SessionAddr` | `zest-proto/src/ids.rs` | **frozen** | WS-F, WS-G, WS-H |
| `ClientMessage`, `HostMessage`, `SessionInfo` | `zest-proto/src/lib.rs` | **frozen** at v3 — see below | WS-F, WS-G |
| `Delta`, `DeltaOp`, `Run`, `RowPayload`, `AttrDef` | `zest-proto/src/delta.rs` | **frozen** at v3 — unchanged in content, but the frame carrying it is now ciphertext | WS-F, WS-G |
| `Nonce32`, `Sig64`, `Pub32`, `AuthFailure` | `zest-proto/src/auth.rs` | **frozen** — `Nonce32`/`Sig64` arrived with v2, `Pub32` with v3 | WS-F, WS-G, WS-H |
| `SecureChannel`, `Sealer`, `Opener`, `EphemeralDh`, `DhPublic` | `zest-mesh/src/secure.rs` | **frozen** at v3 — the browser has a second implementation, pinned to `fixtures/handshake.json` | WS-F, WS-G, WS-H |
| `ClientHandshake`, `Challenge`, `Transcript`, `auth_transcript` | `zest-mesh/src/pairing.rs` | **frozen** at v3 — the transcript layout is signed bytes; a golden pins it | WS-F, WS-G, WS-H |
| `DaemonClient` | `zest-daemon/src/client.rs` | draft — moved down from `zest-app` at v3, see below | WS-A, WS-F |
| `Block`, `BlockIndex`, `BlockState` | `zest-core/src/blocks.rs` | **frozen** — gained `upsert`/`reanchor`, then `started_ms`/`ended_ms` + a caller clock, then `erase_screen`/`authoritative_from`, see below | WS-E, WS-F, WS-G |
| `BlockPayload`, `BlockState` (wire) | `zest-proto/src/delta.rs` | **frozen** — arrived beside `Delta`; gained additive `started_ms`/`ended_ms`, then `Keyframe.blocks_from`, see below | WS-E, WS-F, WS-G |
| `ChangeSource`, `Update`, `update_for` | `zest-core/src/subscribe.rs` | **frozen** — `release_before` removed, see below | WS-F |
| `SessionSource`, `Origin` | `zest-app/src/source.rs` | **frozen** | WS-A, WS-B, WS-F |
| `Peer`, `Endpoint`, `Reachability`, `Discovery` | `zest-mesh/` | **frozen** | WS-F, WS-H |
| `HostIdentity`, `ClientIdentity`, `Signature`, `Nonce`, `Purpose` | `zest-mesh/src/identity.rs` | draft — WS-H may change freely | WS-H only |
| `KeyStore`, `SecretStore`, `CredentialStore` | `zest-mesh/src/keystore.rs` | draft — WS-H may change freely | WS-H, WS-F |
| `DaemonConfig`, `SessionHandle`, `SessionState` | `zest-daemon/src/lib.rs` | draft — WS-F may change freely; gained `min_delta_interval`, see below | WS-F only |
| TypeScript bindings | `crates/zest-proto/bindings/` | **generated** — `cargo xtask check-bindings` | WS-G, WS-H |
| Conformance fixtures | `crates/zest-proto/fixtures/` | **generated** — `cargo xtask check-fixtures` | WS-G, WS-H |
| Settings schema + walked UI fields | `clients/web/packages/settings/generated/` | **generated** — `cargo xtask check-export-web` | WS-G |
| Built-in themes, as TypeScript | `clients/web/packages/theme/src/builtin.generated.ts` | **generated** — `cargo xtask check-export-web` | WS-G |

"Draft" means one stream owns it and nobody else has built on it yet. It freezes when a second
stream starts consuming it.

### Generated artifacts are contracts too

The last four rows are not hand-written and are still seams: they are what a client outside this
repository is *checked against*, which is exactly the property the rest of this table protects.
The bindings and fixtures both carry a `protocol` version, so a `PROTOCOL_VERSION` bump rewrites
every fixture and the change is impossible to miss in review rather than something to remember.

The last two rows are the same idea applied to the things a client *renders* rather than decodes.
`zest-config`'s schema module states the contract in its own words — the settings UIs are
generated from the schema, not hand-listed — and `zest-config::ui` keeps the walk outside the `fs`
feature so a browser can run it. Until `export-web` existed neither actually happened: no
TypeScript read the schema, and the built-in themes reached the browser as hand-copied hex whose
own doc comment named this generator as the fix. A transcription nothing checks is a
transcription that drifts, and the symptom — the native window and the browser disagreeing about
what `obsidian` looks like — is invisible until someone puts them side by side.

They are also where this file stops being aspirational about the web client. The rule above —
land a contract change with every consumer in one commit — now has an in-repo consumer that will
fail loudly: `clients/web/` decodes these fixtures frame by frame. A wire change that regenerates
them without updating the TypeScript no longer merges green.

The division of labour between them is worth stating, because it is why both exist. The bindings
say what the wire **looks like**, and catch a shape that moved. The fixtures say what it
**means**, and catch a client that decodes the right shapes and applies them wrongly — a scroll
in the wrong order, a run's cell count recomputed instead of read. No type check can reach the
second kind.

---

## The five things that are cheap now and a rewrite later

Three of these predate the fleet and are already load-bearing. Two came with it. None may be
undone without an ADR arguing against the reasoning.

1. **Premultiplied alpha and the offscreen resolve pass.** Retrofit = rewrite every shader.
   → ADR-003.
2. **`GlyphInstance` carries absolute physical-pixel position and RGBA colour**, not `(row, col)`
   and a palette index. Retrofit = rewrite the glyph pipeline and every call site. This is what
   lets chrome text, tab titles, the command palette and block headers share the grid's atlas.
3. **`render(&[Viewport], &Chrome)`**, even though M1 passes one viewport. Retrofit = restructure
   the render loop for panes.
4. **Every session is addressed `(HostId, SessionId)`** from the first protocol byte. Retrofit =
   a protocol version bump and a change to every client, released separately. → ADR-006.
5. **`zest-app` reaches sessions only through `SessionSource`.** Retrofit = rewriting the event
   loop, which is where chrome, motion and input all land — a conflict with three streams at once.
   → ADR-007.

---

## Deliberately not abstracted

Worth stating, because each looks like an omission:

**`Terminal` itself.** The renderer reads the grid directly under a lock; a trait between the
renderer and the cells would either allocate per frame or force an iterator that defeats the
50–150µs extract. A *remote* session keeps a real local `Terminal` that deltas are applied into,
so the renderer's path is identical at both ends of the mesh.

**A transport trait in `zest-proto`.** The wire types know nothing about sockets. Routing lives in
`zest-mesh`, so a protocol change and a transport change can never be the same commit.

**`has_exited` on `SessionSource`.** Nothing calls it — exit arrives as a `Wakeup::Exited` event.
A contract that three streams implement should carry only what is used; speculative methods are
how an interface becomes something people satisfy without knowing why.

**`release_before` on `ChangeSource`** — removed after the fact, by its only consumer. It assumed
the terminal would retain a delta history that subscribers acknowledged their way through. The
encoder instead keeps a shadow of what each subscriber last saw and diffs against it, so there is
no shared history and nothing to release. A memory-management method that manages nothing tells
every caller memory *is* being managed, which is precisely the belief that stops someone checking.
The property it protected now holds by construction: per-subscriber state disappears with the
subscriber.

---

## Changing one anyway

1. Write down what breaks and why the current shape cannot serve. A comment on the master plan
   (#1) is enough; the point is that the reasoning outlives the diff.
2. Name every consumer — the table above is the list, and it must be *complete* before you start,
   not discovered while compiling.
3. Land the change and every consumer in one commit. A frozen contract with a half-updated
   consumer is worse than either shape.
4. Update the table in the same commit. A row that no longer describes the code is worse than no
   row, because it is believed.

### Additive, and therefore not a bump: command blocks

`Delta` gained `blocks`, `HostMessage::Keyframe` gained `blocks`, and
`BlockPayload`/`BlockState` arrived beside them. All `#[serde(default)]`, so a
peer that predates them decodes exactly as before — it simply has no semantic
view of the session, which is what every peer had until now. `PROTOCOL_VERSION`
stays at 2.

**A field rather than a `DeltaOp` variant, and that is the whole design.**
`DeltaOp` is `#[serde(tag = "op")]`: an unknown tag fails the *whole* `Delta`,
not just the op, so a new variant is not additive and would have forced a
version bump on its own. It would also have needed a third ordering invariant
beside `scrolls_come_first` and `screen_switch_comes_first`. Block upserts are
keyed and order-independent — they are applied after the ops because they name
line ids the rows in the same batch establish, and that is a rule about the
batch, not about the ops within it.

`BlockPayload` mirrors `zest_core::Block` rather than re-exporting it, for the
same reason `RowPayload` mirrors a `Row`: the wire type carries a `ts_rs` derive
and `zest-core` must keep building for `wasm32`. Line ids are `i64` to match
`RowPayload::line`, because a client compares the two.

**Two additions to the frozen `BlockIndex`**, neither of which changes an
existing signature: `upsert`, which is the client half — a remote session's
blocks are parsed on the machine the shell runs on and arrive whole, so there
are no markers to replay — and `reanchor`, which maps blocks through the
`Reindex` a width change produces.

**Eviction deliberately has no wire message.** A client evicts on its own
scrollback bound through the same code the host uses, so a client configured to
keep more history than the host keeps more, rather than being told to forget.

**Destruction is not eviction, and needed one** (#124). `ESC[2J` erases the rows
a block describes, so the host drops it — but line ids survive an erase and the
shell reuses the very ids the old blocks still claim, and a stale block *has* an
`output_line` where a fresh prompt does not, so the header pass drew the old
command over the row the user was typing on. Opaque, and it ate the click too.

That fix is invisible without the wire: the window is a client of its own
daemon, `diff_blocks` cannot say "removed", and `Applier::apply_keyframe`
upserted rather than replaced, so even a keyframe left the stale block. So
`Keyframe` gained `blocks_from: u32`, `#[serde(default)]` — the id from which
the carried list is complete. The applier drops what it holds from there up
before inserting; below it is the client's own longer history, which the
paragraph above says it keeps.

The value is `BlockIndex::authoritative_from`, which **rises** past what
eviction took and **falls** to what a clear destroyed. One number rather than
two because an empty list otherwise cannot distinguish "the host has evicted
everything, keep yours" from "the host destroyed everything, drop yours" — and
`cls` on a fresh session is exactly the second. `Encoder::blocks_need_keyframe`
tells the daemon to resync when a removal is not a prefix trim; a keyframe is
the right price, since the whole screen just changed. Default 0 means an older
host replaces wholesale, which is what the browser's `GridView` already did.
Landed with `zest-proto`, the daemon, the app and `clients/web` in one commit.

**Additive once more, for the block headers (design screen 3): timestamps.**
`Block` and `BlockPayload` gained `started_ms`/`ended_ms` — wall-clock
milliseconds since the Unix epoch, `#[serde(default)]` on the wire, so an old
host simply sends blocks without times and the header omits its duration.
Wall time, not a monotonic instant, because "2m ago · exit 0" is computed on
whichever device is looking. **Start stamps, never a live elapsed**:
`diff_blocks` resends a block whenever any field differs, and an elapsed field
would resend every running block on every tick; readers subtract instead.
Two `BlockIndex` signatures changed with it — `begin_output` and `finish` take
`now_ms: Option<u64>` — because `zest-core` is `no_std` and has no clock: the
embedder states the time (`Terminal::set_now_ms`) where bytes and wall time
meet, which is the pty reader in both the daemon and the in-process fallback.
Landed with every consumer in one commit, per the rule below.

### Additive again, for the tab strip: titles, created, watch_sessions

Three fields, all `#[serde(default)]`, `PROTOCOL_VERSION` still 2 (#23):

- **`Keyframe.title`** (wire and `encode::Keyframe`). The title was the one
  piece of a complete state that only travelled as a *change*
  (`DeltaOp::Title`), so a client attaching to a session already titled `vim`
  showed blank until the host next retitled. Empty means untitled and travels
  as absent, so an older host's keyframe leaves the client's title alone
  rather than blanking it.
- **`HostMessage::Sessions.created`** — the session a `CreateSession` in this
  reply produced. Retires the client's `sessions.last()` heuristic, which
  hands one of two concurrent creators the other's shell. Absent on listings
  and pushes; a client talking to an older daemon falls back to the
  heuristic, racy exactly as it always was.
- **`ClientMessage::Hello.watch_sessions`** — opt in to hearing `Sessions`
  pushes whenever this host's listing changes (create, close, collection,
  attach, detach — coalesced through a registry generation counter). A field
  rather than a new message for the same reason blocks were a field: both
  enums are tagged and an unknown tag fails the whole message. Opt-in rather
  than broadcast because an old client would mistake an unsolicited
  `Sessions` for the reply to a request it is about to make. A client that
  asked and hears nothing is talking to an older daemon and polls instead.

### Done once, deliberately: protocol 2

Two changes a peer cannot ignore, made as one bump because the coordinated moment is the
expensive part:

- **A challenge/response handshake.** A signature carried on `Hello` alone proves nothing that
  survives being recorded, because the client picks every byte it signs. The host has to
  contribute freshness, which costs a round trip and two new messages.
- **`DeltaOp::Modes`.** A client encodes its own keystrokes — that is what `Input` carries — and
  cannot do it correctly without the host's mode bits. `APP_CURSOR` alone decides whether an arrow
  key is `ESC [ A` or `ESC O A`. Without it an attached session had no mouse reporting, no
  bracketed paste, and broken arrows in every full-screen program.

Neither could ride on `serde`'s tolerance of unknown fields. **An auth field a peer may ignore is
an auth field that does not exist**, and a mode a client never receives is not a degraded
experience but a broken one.

The whole consumer set at the time was `zest-daemon`, its tests and its `attach` example — three
files. Once a web or phone client ships, the same change is a release across three codebases gated
on an app-store cycle. That asymmetry is the argument for doing it now rather than later, and for
doing it *once*.

### Done once, deliberately: protocol 3

One change a peer cannot ignore, and this time it is not a field but the *meaning of the bytes*.
Since v3 everything after the `Challenge` is sealed with ChaCha20-Poly1305 (ADR-008). A v2 peer
hands ciphertext to `rmp_serde` and reports "message was not understood" — the wrong-layer
diagnosis that `Hello.nonce`'s `#[serde(default)]` exists to prevent — so there was no serde
attribute that could express it. No attribute can express "the bytes are now opaque".

Three things moved together, because a half-updated one of them is worse than any of them:

- **`Hello.dh` and `Challenge.dh`**, two `Pub32`s. `#[serde(default)]` on both, which is *not* an
  attempt at compatibility — the version check refuses a v2 peer regardless. It is so the peer
  reaches that check with a decodable message and hears "protocol 2 is not compatible with 3"
  instead of a parse error naming nothing.
- **The transcript layout**, which gained both DH keys and became fallible. It refuses an
  unencodable label rather than truncating at 65535 bytes: two labels sharing that prefix used to
  sign identical bytes, so a signature over one was valid for the other, and a label is
  attacker-influenced and is the entire text of the approval prompt. (#43.)
- **`frame::encode` split** into `encode_body` and `frame_bytes`, so a sealer has somewhere to
  stand. The `u32` LE prefix now describes the *ciphertext*, which is 16 bytes longer than the
  plaintext — so the size bound moved to the sealer. A bound left on the plaintext passes every
  small test and fails only on a maximal keyframe, which is to say only on very large grids.

**The consumer set was larger than v2's, and one part of it was not a consumer at all.** Nine
places inside `zest-daemon` hand-rolled the client half of the handshake — its tests, its `attach`
and `pair` examples, its loopback test. That was survivable while a wrong peer failed loudly at
the signature; it stopped being survivable when the same steps derive the key everything
afterwards is encrypted under. So `DaemonClient` moved down from `zest-app` into `zest-daemon` and
all nine now use it. One implementation, exercised by the app *and* by every diagnostic.

`fixtures/handshake.json` is new and is the reason a second implementation is possible at all —
see ADR-008.

### Added once, deliberately: `PtyTransport::hangup`

A session could be created and never ended. `CloseSession` removed the registry entry and dropped
the transport, which reads like a teardown and is not one:

- On unix the hangup fires when the **last** duplicate of the master fd closes, and a reader thread
  parked in `read` is holding one. It cannot release it until the read returns, and the read will
  not return until the hangup. Nothing outside that cycle breaks it, and every call involved
  reports success.
- On Windows dropping *is* the documented protocol and does work — but only if the `Arc` being
  dropped is the last one, which the session layer cannot promise while a concurrent listing or
  poll may hold a clone.

So the trait grew one method rather than the daemon growing a workaround. The alternative — having
`Registry::close` reach for a `NativePty` concrete type — would have put a `#[cfg]` in the daemon
for something that is not a platform question but a lifecycle one.

Both implementations escalate on the same timer: ask (`SIGHUP` to the session's process group,
`ClosePseudoConsole`), wait 150ms, insist (`SIGKILL`, `TerminateProcess`). A shell gets to run its
exit traps; a program that declines to leave still goes.

**`hangup` is not called on `Detach`,** and that asymmetry is the whole of ADR-007. Consumers at
the time: `zest-daemon` only — `zest-app` reaches a pty exclusively through `SessionSource`.

*The Windows half has been run, and then made to mean the same thing as the unix one.* It was first
exercised by hand (#18): `attach --close` ended the child and the daemon survived
`ClosePseudoConsole`, while the same attach without `--close` left the shell running — so ADR-007's
asymmetry holds on ConPTY. What that did **not** cover is now fixed rather than merely noted: the
escalation terminated a *job object* instead of the bare process, because `TerminateProcess` reaches
the process it names and nothing else, so a shell's detached grandchildren used to survive a hangup
that unix's `SIGHUP`-to-the-process-group reaps. `hangup_ends_everything_the_shell_started` exists on
both platforms now, and on Windows it fails without the job.

A related gap closed with it: `PtyTransport::watch_exit`, whose default no-op meant a shell that
exited *on its own* was never noticed on Windows at all. → #18.

---

### Changed once, to close a hole: `DaemonClient::into_halves` returns `Halves`

It returned the tuple `(read, write, channel)` and dropped `self.frames` on the floor. That field
is not spare capacity: `recv` reads up to 64 KiB and returns **one** message, keeping the rest, and
the daemon's writer loop batches a whole wake into back-to-back writes with a single `flush`. So a
socket that coalesces the attach reply with what follows leaves real messages in that buffer, and
the handoff deleted them.

Under the sealed channel that is not "one lost update". The nonce is an implicit per-direction
counter (ADR-008), so a dropped frame puts the two sides permanently out of step and every later
frame fails to open. The window is blank and stays blank — issue #54.

`into_halves` now returns a named `Halves { read, write, channel, frames }`. A **struct rather than
a tuple, because `frames` is the field a caller forgets**: destructuring a tuple by position made
dropping it invisible, and a named field that is ignored is at least ignored in writing. Both attach
paths in `zest-app/src/remote.rs` seed their streaming `FrameReader` from it.

**Carrying the buffer is only half of it**, which is worth stating because the first fix stopped
there and passed its own test. The reader loop blocked on `read()` before draining what it had been
handed, so a session that goes quiet after the coalesced burst — a command that prints and exits,
which is not exotic — still lost everything. The carried frames are drained *before* the first
blocking read.

The test that pins it asserts on `Wakeup::Exited`, not on the grid, and that choice is load-bearing:
a command short enough to finish during a stalled attach has its output in the *keyframe*, so the
carried `Update` is redundant and losing it is invisible. `Exited` exists in exactly one frame with
no understudy. The test fails if the carry is reverted **and** if the drain is reverted.

Consumers: `zest-app` only. Still draft — WS-A and WS-F may move it — but the shape now has a reason
that should survive the next change.

---

### Added once, for the relay: `DaemonConfig::min_delta_interval`

A least interval between *delta* sends on one connection. **Zero by default**, so loopback and the
LAN are byte-for-byte what they were; the relay transport sets ~30ms, because incoming messages are
billed and a Durable Object that never idles never hibernates. → ADR-009.

**A floor is safe here for a reason that is a property of this protocol and not of throttling in
general.** `zest-proto` coalesces on *state*: a subscriber holds an encoder shadow and asks the
terminal for the difference from what it last sent, so a consumer that skips a hundred polls
receives one delta describing the current grid rather than a backlog of a hundred. Nothing queues,
so nothing is lost by not looking. The same throttle over a byte stream would drop the bytes it
skipped. → ADR-004.

Two things it must never delay, and both are tested rather than asserted: a session's `Exited` —
a client that is not told its shell ended waits for output that is not coming — and a keyframe
answering `Attach` or `RequestKeyframe`, which are replies and never pass through the throttled
poll. Skipping also has to mean *not asking the session at all*: `Session::poll` advances the
subscriber's baseline, so a poll whose answer is discarded destroys output rather than coalescing
it.

Consumers: `zest-daemon` only. **It is no longer inert**: `relay::pipe_config` sets 30ms on every
relay pipe and on nothing else, which is the one consumer it was added for. Still draft — WS-F may
move it.

---

### The relay control link: JSON, and the one seam that is not `zest-proto`

Four messages between `zest-daemon` and the relay Worker, and neither end shares a line of code with
the other. `crates/zest-daemon/src/relay.rs` and `cloud/packages/relay/src/room/control.ts` are the
two implementations.

```
DO -> {"t":"challenge","v":1,"nonce":"<64 hex>","relay_key":"<64 hex>"}
D  -> {"t":"hello","v":1,"host":"<64 hex>","label":"…","sig":"<128 hex>"}
DO -> {"t":"ready","v":1}   |   {"t":"error","v":1,"code":"…"}
DO -> {"t":"open","v":1,"pipe":"<32 hex>","exp":<epoch ms>}
```

**Text, not MessagePack, and that is not a lapse.** The object's free keepalive is
`WebSocketRequestResponsePair('ping','pong')`, whose members are strings, and answering a ping
without waking the object is the whole of ADR-009's "an idle host costs nothing". The pipes this
link opens carry `zest-proto` as binary and are unaffected.

`sig` is an ordinary `Role::Host` + `Purpose::Auth` signature over the nonce bytes under the
`zesterm-sig-v1` preimage, so there was nothing new to implement on either side. Both ends pin it
against the *same* Rust-produced vector — `relay::tests::the_hello_this_daemon_sends_is_the_one_the_relay_pinned`
and `cloud/packages/relay/test/control-golden.test.ts` — because a byte of drift otherwise arrives at
bring-up as a daemon that is refused and a Worker that says the signature is bad, with neither able
to say which of them moved.

Two fields are deliberately **not** acted on, and a reader should know before assuming otherwise.
`relay_key` is read and logged and pinned to nothing; the Worker cannot prove it yet either
(`relay/src/env.ts` says so at length), and what makes it survivable is that everything of value
inside a pipe is sealed to a key the relay never holds. `exp` is read and not enforced: it is
absolute epoch milliseconds, so honouring it would mean trusting this machine's clock against the
relay's, and a laptop ten seconds out would silently refuse every attach it was ever offered. The
relay enforces its own deadline — a late dial gets a 404 — which depends on nobody's clock.

An unknown `t` is ignored rather than fatal, so a fifth message cannot knock every daemon in the
fleet off its link on deploy. A `v` that is not 1 is refused by name on both sides.

Consumers: `zest-daemon` and `cloud/packages/relay`. Frozen in the sense that matters — the two
implementations are already deployed against each other's tests.

### Additive again, for the profiles editor: four `Widget` variants

`zest_config::ui::Widget` is a multi-consumer contract — its kebab-case
serde names are mirrored by hand in
`clients/web/packages/settings/src/fields.ts`, and `check-export-web` only
catches drift in the *generated* files, not that union. The §12 profiles
editor added `host-picker`, `scheme-picker`, `accent-picker` and
`icon-picker` (#135). They appear only in `profiles::fields()`, which is
hand-authored and not part of the schema walk, so `ui-fields.json` did not
change and old web clients decode nothing new — but any future
`profile-fields.json` export must land the fields.ts union update in the
same PR, or the browser's settings package fails to typecheck against a
widget it has never heard of.

The create-session frame's `cwd` deserves a line for what did *not* happen:
the §12 launch work (#178) needed a working directory on the wire and found
the field already present and daemon-consumed since the frame was born — no
growth, no bump, one new round-trip test pinning a `\wsl$` path through
the real framing. Check for the field you need before growing a frame.

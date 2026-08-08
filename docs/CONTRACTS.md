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
| `PtyTransport` | `zest-pty/src/lib.rs` | **frozen** | WS-C, WS-D |
| `HostId`, `ClientId`, `SessionId`, `SessionAddr` | `zest-proto/src/ids.rs` | **frozen** | WS-F, WS-G, WS-H |
| `ClientMessage`, `HostMessage`, `SessionInfo` | `zest-proto/src/lib.rs` | **frozen** at v2 — see below | WS-F, WS-G |
| `Delta`, `DeltaOp`, `Run`, `RowPayload`, `AttrDef` | `zest-proto/src/delta.rs` | **frozen** at v2 — see below | WS-F, WS-G |
| `Nonce32`, `Sig64`, `AuthFailure` | `zest-proto/src/auth.rs` | **frozen** — arrived with v2 | WS-F, WS-G, WS-H |
| `Block`, `BlockIndex`, `BlockState` | `zest-core/src/blocks.rs` | **frozen** | WS-E, WS-F, WS-G |
| `ChangeSource`, `Update`, `update_for` | `zest-core/src/subscribe.rs` | **frozen** — `release_before` removed, see below | WS-F |
| `SessionSource`, `Origin` | `zest-app/src/source.rs` | **frozen** | WS-A, WS-B, WS-F |
| `Peer`, `Endpoint`, `Reachability`, `Discovery` | `zest-mesh/` | **frozen** | WS-F, WS-H |
| `HostIdentity`, `ClientIdentity`, `Signature`, `Nonce`, `Purpose` | `zest-mesh/src/identity.rs` | draft — WS-H may change freely | WS-H only |
| `KeyStore` | `zest-mesh/src/keystore.rs` | draft — WS-H may change freely | WS-H only |
| `DaemonConfig`, `SessionHandle`, `SessionState` | `zest-daemon/src/lib.rs` | draft — WS-F may change freely | WS-F only |
| TypeScript bindings | `crates/zest-proto/bindings/` | **generated** — `cargo xtask check-bindings` | WS-G, WS-H |
| Conformance fixtures | `crates/zest-proto/fixtures/` | **generated** — `cargo xtask check-fixtures` | WS-G, WS-H |

"Draft" means one stream owns it and nobody else has built on it yet. It freezes when a second
stream starts consuming it.

### Generated artifacts are contracts too

The last two rows are not hand-written and are still seams: they are what a client outside this
repository is *checked against*, which is exactly the property the rest of this table protects.
Both carry a `protocol` version, so a `PROTOCOL_VERSION` bump rewrites every fixture and the
change is impossible to miss in review rather than something to remember.

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

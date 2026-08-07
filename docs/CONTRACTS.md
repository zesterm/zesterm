# Frozen contracts

Several workstreams are being built at once, by different people, against shared seams. A seam
that moves while three streams are building on it costs more than the change was worth.

**The rule: a stream may not change a frozen contract. It opens an issue and waits.**

That is not bureaucracy — it is the only thing that makes parallel work cheaper than serial work.
A stream that quietly widens a trait to suit itself has just created a merge conflict for everyone
else, and the person who finds it is whoever merges last.

Adding a *new* type next to a frozen one is always fine. Adding a `#[serde(default)]` field is
fine. Changing a signature, renaming a variant, or removing a method is not.

---

## The seams

| Contract | Where | Status | Consumed by |
|---|---|---|---|
| `PtyTransport` | `zest-pty/src/lib.rs` | **frozen** | WS-C, WS-D |
| `HostId`, `ClientId`, `SessionId`, `SessionAddr` | `zest-proto/src/ids.rs` | **frozen** | WS-F, WS-G, WS-H |
| `ClientMessage`, `HostMessage`, `SessionInfo` | `zest-proto/src/lib.rs` | **frozen** | WS-F, WS-G |
| `Delta`, `DeltaOp`, `Run`, `RowPayload`, `AttrDef` | `zest-proto/src/delta.rs` | **frozen** | WS-F, WS-G |
| `Block`, `BlockIndex`, `BlockState` | `zest-core/src/blocks.rs` | **frozen** | WS-E, WS-F, WS-G |
| `ChangeSource`, `Update`, `update_for` | `zest-core/src/subscribe.rs` | **frozen** — `release_before` removed, see below | WS-F |
| `SessionSource`, `Origin` | `zest-app/src/source.rs` | **frozen** | WS-A, WS-B, WS-F |
| `Peer`, `Endpoint`, `Reachability`, `Discovery` | `zest-mesh/` | **frozen** | WS-F, WS-H |
| `HostIdentity`, `ClientIdentity`, `Signature`, `Nonce`, `Purpose` | `zest-mesh/src/identity.rs` | draft — WS-H may change freely | WS-H only |
| `KeyStore` | `zest-mesh/src/keystore.rs` | draft — WS-H may change freely | WS-H only |
| `DaemonConfig`, `SessionHandle`, `SessionState` | `zest-daemon/src/lib.rs` | draft — WS-F may change freely | WS-F only |

"Draft" means one stream owns it and nobody else has built on it yet. It freezes when a second
stream starts consuming it.

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

1. Open an issue against the master plan (#1) describing what breaks and why the current shape
   cannot serve.
2. Name every stream that consumes it — the table above is the list.
3. Land the change and every consumer in one commit. A frozen contract with a half-updated
   consumer is worse than either shape.

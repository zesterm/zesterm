# Frozen contracts

A frozen contract is a seam with consumers that do not ship on this repository's
schedule — the web client, the phone, and every already-paired daemon in a fleet
that upgrades one machine at a time. A wire type that moves casually breaks
clients that cannot be recompiled alongside it.

**The rule:** a frozen contract does not move casually, and never moves half-way.
To change one — land it with **every** consumer in the same PR, and update the
table below in that same PR. A frozen contract with a half-updated consumer is
worse than either shape.

Adding a *new* type next to a frozen one is always fine. Adding a
`#[serde(default)]` field is fine. Changing a signature, renaming a variant, or
removing a method is a deliberate act with a paragraph of justification attached
— put the paragraph in the PR (or its issue), where the reasoning outlives the
diff. Past changes and their justifications live in git history; this file
describes only the current shape.

---

## The seams

| Contract | Where | Status | Consumed by |
|---|---|---|---|
| `PtyTransport` | `crates/zest-pty/src/lib.rs` | **frozen** — includes explicit `hangup` and `restates_on_resize` | `zest-daemon`, both platform ptys |
| `HostId`, `ClientId`, `SessionId`, `SessionAddr` | `crates/zest-proto/src/ids.rs` | **frozen** | `zest-daemon`, `zest-mesh`, `zest-app`, `zest-mcp`, `clients/web` |
| `ClientMessage`, `HostMessage`, `SessionInfo`, `HostOffer`, `HostProfile` | `crates/zest-proto/src/lib.rs` | **frozen** at v3, additively extended (`watch_pairings`, `watch_hosts`, `watch_signals`, `Sessions.offer`, `Attach.observe`, `HostMessage::Attention`, `HostMessage::Progress`, the loopback-scoped `Enroll`/`EnrollResult` pair, the cwd browser's `ListDir`/`DirListing` pair (#439), `HostOffer.has_account_token`, `SessionInfo.context`/`busy` with the `SessionContext` family beside it, #416; the editor's `ReadFile`/`FileContents` and `WriteFile`/`FileWritten` pairs, #446, and `GitDiff`/`GitDiffResult` beside them, #453; `CreateSession.env`, #488 — skipped when empty, so an ordinary launch is byte-identical to what a peer predating it sent and the conformance fixtures do not move; `Hello.agent`, a client declining device authority for itself, #491). **A new `HostMessage` tag is only additive behind a `Hello` flag** — an older peer cannot decode the frame and `DaemonClient::recv` maps that to a transport error, which ends the connection; `watch_signals` is what keeps `Attention` from reaching one. **The exception is a tag that is only ever a *reply*** — `EnrollResult`, `DirListing`, `FileContents`, `FileWritten`, `GitDiffResult` — which reaches a peer only in answer to a request that peer had to send first, so a peer too old to decode it is structurally unable to ask for it; those need no flag, and the matching `ClientMessage` tag degrades through the daemon's generic could-not-understand `Error` | `zest-daemon`, `zest-app`, `zest-mcp`, `clients/web` |
| `Delta`, `DeltaOp`, `Run`, `RowPayload`, `AttrDef` | `crates/zest-proto/src/delta.rs` | **frozen** at v3 — the frame carrying it is ciphertext | `zest-daemon`, `zest-app`, `zest-mcp`, `clients/web` |
| `Nonce32`, `Sig64`, `Pub32`, `AuthFailure` | `crates/zest-proto/src/auth.rs` | **frozen** | `zest-daemon`, `zest-mesh`, `zest-app`, `clients/web` |
| `SecureChannel`, `Sealer`, `Opener`, `EphemeralDh`, `DhPublic` | `crates/zest-mesh/src/secure.rs` | **frozen** at v3 — the browser has a second implementation, pinned to `fixtures/handshake.json` | `zest-daemon`, `clients/web` |
| `ClientHandshake`, `Challenge`, `Transcript`, `auth_transcript` | `crates/zest-mesh/src/pairing.rs` | **frozen** at v3 — the transcript layout is signed bytes; a golden pins it | `zest-daemon`, `zest-app`, `clients/web`, `cloud/` |
| `DaemonClient` | `crates/zest-daemon/src/client.rs` | **frozen** — a second local client (`zest-mcp`) built on it. `Watch` grew `signals` and `connect_with` now takes a `Watch` rather than a bare `watch_sessions` flag (#383): both consumers are in this repository and moved in the same PR, and the alternative was a fourth constructor differing from `connect_watching` by one argument. `create` and `open_session` grew a launch `env` (#488), both consumers moving in the same PR: without it the daemon had no way to be told a session's environment, and `shell.env` applied on no path anyone takes. `connect_with` then grew `ClientKind` on the same justification (#491) — **not** a field on `Watch`, whose doc says it holds what a connection *subscribes to*, and declaring what a client *is* is not a subscription | `zest-app`, `zest-mcp` |
| `find_or_spawn`, `Attached`, `DaemonStartError`, `resolve_daemon_binary` | `crates/zest-daemon/src/spawn.rs` | **frozen** — same second consumer | `zest-app`, `zest-mcp` |
| `Block`, `BlockIndex`, `BlockState` | `crates/zest-core/src/blocks.rs` | **frozen**, additively extended: `Block.context` (`BlockContext`, embedder-stamped, #429), `Block.author` (32 opaque bytes, embedder-stamped, #491 — **not** a `ClientId`: `zest-proto` depends on this crate, so the wire converts, as `LineId` becomes `i64` there) | `zest-app`, `zest-daemon`, `zest-mcp`, `clients/web` |
| `BlockPayload`, `BlockState` (wire) | `crates/zest-proto/src/delta.rs` | **frozen**, additively extended: `context` (`BlockContextPayload`, `#[serde(default)]`, #429), `author` (`Option<ClientId>`, hex, `#[serde(default)]`, #491) | `zest-daemon`, `zest-app`, `zest-mcp`, `clients/web` |
| `ChangeSource`, `Update`, `update_for` | `crates/zest-core/src/subscribe.rs` | **frozen** | `zest-daemon` |
| `SessionSource`, `Origin` | `crates/zest-app/src/source.rs` | **frozen**, additively extended: `predict`/`predicted` with default bodies (#442) — a source that never guesses implements neither, and the renderer's path is untouched for it | `zest-app` |
| `Peer`, `Endpoint`, `Reachability`, `Discovery` | `crates/zest-mesh/` | **frozen** | `zest-daemon`, `zest-app` |
| `HostIdentity`, `ClientIdentity`, `Signature`, `Nonce`, `Purpose` | `crates/zest-mesh/src/identity.rs` | draft — `zest-mesh` may change it freely | `zest-mesh`, `zest-daemon` |
| `Attestation`, `attestation_message`, `sign_attestation`, `verify_attestation`, `decode_attestation` | `crates/zest-mesh/src/attest.rs` | **frozen** — the `zesterm-attest-v1` layout and its `base64url(message).base64url(sig)` framing; the TS port (`cloud/packages/shared/src/attestation.ts`) and `crates/zest-daemon/src/attest_sync.rs` are both pinned to `fixtures/attest.json` | `zest-daemon`, `cloud/` |
| `KeyStore`, `SecretStore`, `CredentialStore` | `crates/zest-mesh/src/keystore.rs` | draft — `zest-mesh` may change it freely | `zest-mesh`, `zest-daemon` |
| `key::encode`, `key::encode_press` — the legacy keystroke encoding | `crates/zest-input/src/key.rs` | **frozen** — two ports pinned to it: `clients/web/packages/input/src/key.ts` (by review, and it says so in its header) and `crates/zest-mcp/src/keys.rs` (by `crates/zest-mcp/tests/keys.rs`, byte-for-byte over every name × modifier × DECCKM state). `encode_press` exists because `winit::KeyEvent` has a private platform tail and cannot be built outside winit, so no external test could reach the encoder at all | `zest-app`, `zest-mcp`, `clients/web` |
| `FleetHost`, `SessionsState`, `HostRoute`, `best_route`, `AccountEntry`, `merge_account` | `crates/zest-fleet/src/lib.rs` | **frozen** — `zest-mcp` became the second consumer (#274), which is what this row said would freeze it. Every field stays `pub` and the struct keeps no private state: `zest-mcp` builds these rows itself from mDNS and the account listing rather than receiving them from a `FleetModel`, and `fixture` is how both consumers' tests build one — which is why `merge_account` came down beside the rule (#274): assembling a row is the *other* half of that job, and `AccountEntry` names the three facts it merges on rather than the transport's `AccountHosts`, so the decision still cannot dial | `zest-app` |
| `AccountApi`, `AccountHost`, `AccountHosts`, `AccountDevice`, `fetch_hosts`, `mint_ticket`, `relay_dial`, `relay_dialer`, `RelayDialError`, `CredentialRefusal`, `stored_app_token` | `crates/zest-daemon/src/account.rs` | **frozen** — two consumers since #274 (`zest-app`, `zest-mcp`). It lived in `zest-app`, whose `[[bin]]`-only manifest made it unreachable; the constraint to preserve is that **`relay_dialer` stays the only ladder** — a second copy of token → ticket → TLS is where a reused ticket or a captured credential gets written | `zest-app`, `zest-mcp` |
| `DaemonConfig`, `SessionHandle`, `SessionState` | `crates/zest-daemon/src/lib.rs` | draft — daemon-internal, may change freely | `zest-daemon` |
| `SessionEntry`, `HostInfo`, `DataPlane`, `HostFacts`, `LaunchTarget`, `DirectoryView` | `clients/web/packages/control/src/session-directory.actor.ts` | **frozen** — the actor wire between the sidecar (which writes) and the app (which reads); JSON, and deliberately free of `@zesterm/proto` (see below). Additively extended: `busy` and `context` (`EntryContext`/`EntryGit`/`EntryFact`, the `SessionContext` projection, #416) | `clients/web` |
| Cloud enrolment HTTP bodies — `/api/enroll/claim`, `/api/devices/register`, `/api/link/{start,claim}` requests, success envelopes and refusal shapes (`{error, detail?}`) | `cloud/packages/web/src/api/{enroll,devices,link}.ts` | **frozen** — de facto: deployed `zest-daemon` and `zest-app` binaries parse these bodies (`crates/zest-daemon/src/enroll.rs`, `crates/zest-app/src/cloud.rs`) and cannot be recompiled alongside the Worker; additive fields fine (`detail` on the 409 is one, #367) | `zest-daemon`, `zest-app`, `clients/web` |
| TypeScript bindings | `crates/zest-proto/bindings/` | **generated** — `cargo xtask check-bindings` | `clients/web`, `cloud/` |
| Conformance fixtures | `crates/zest-proto/fixtures/` | **generated** — `cargo xtask check-fixtures` | `clients/web` |
| `Predictor`, `Key`, `Policy` | `crates/zest-proto/src/predict.rs` | **frozen rules, not a wire type** — the echo predictor has a second implementation (`clients/web/packages/proto/src/predict.ts`); both replay the hand-authored `fixtures/predict.json`, so a rule change is a fixture change (ADR-016) | `zest-app`, `clients/web` |
| Settings schema + walked UI fields | `clients/web/packages/settings/generated/` | **generated** — `cargo xtask check-export-web`. The `window.background_*` keys are carried and deliberately unread there (#450): a browser cannot open a native host's path | `clients/web` |
| Built-in themes, as TypeScript | `clients/web/packages/theme/src/builtin.generated.ts` | **generated** — `cargo xtask check-export-web` | `clients/web` |

"Draft" means it has a single consumer and may still change freely. It freezes
when a second consumer builds on it — which is how `DaemonClient` and
`find_or_spawn` froze, when `zest-mcp` became their second caller.

### The control actors carry a projection, not the wire shape

`@zesterm/control` depends on `@sigx/actors` and nothing else, and its actor
wire is `@sigx/serialize` JSON. So the daemon's types stop at that boundary and
are projected: `SessionInfo` → `SessionEntry`, `HostOffer` → `HostFacts`,
`HostProfile` → `LaunchTarget`, each by a function beside the type
(`sessionEntryOf`, `hostFactsOf`) whose parameter is **structural** rather than
imported.

The reason: a dependency added for one argument's type is a dependency every
actors host then carries — including a Bun sidecar that has no business
knowing what msgpack is. (`SessionId` used to be the second reason, when its
binding said `bigint` — JSON cannot carry one. Since #14 it decodes as a plain
`number`; the id still crosses the actor wire as a string because that is what
this frozen seam already says, and the UI only compares and displays it.)

They are renamed rather than sharing a name because one name for two
representations is how a `snake_case` field ends up read off a `camelCase`
object — a mistake that typechecks in neither direction only while the names
differ.

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

The rule above — land a contract change with every consumer in one PR — has an in-repo consumer
that will fail loudly: `clients/web/` decodes these fixtures frame by frame. A wire change that
regenerates them without updating the TypeScript no longer merges green.

The division of labour between them is worth stating, because it is why both exist. The bindings
say what the wire **looks like**, and catch a shape that moved. The fixtures say what it
**means**, and catch a client that decodes the right shapes and applies them wrongly — a scroll
in the wrong order, a run's cell count recomputed instead of read. No type check can reach the
second kind.

---

## The five things that are cheap now and a rewrite later

None may be undone without an ADR arguing against the reasoning.

1. **Premultiplied alpha and the offscreen resolve pass.** Retrofit = rewrite every shader.
   → ADR-003.
2. **`GlyphInstance` carries absolute physical-pixel position and RGBA colour**, not `(row, col)`
   and a palette index. Retrofit = rewrite the glyph pipeline and every call site. This is what
   lets chrome text, tab titles, the command palette and block headers share the grid's atlas.
3. **`render(&[Viewport], &Chrome)`**, even though the app passes one viewport today. Retrofit =
   restructure the render loop for panes.
4. **Every session is addressed `(HostId, SessionId)`** from the first protocol byte. Retrofit =
   a protocol version bump and a change to every client, released separately. → ADR-006.
5. **`zest-app` reaches sessions only through `SessionSource`.** Retrofit = rewriting the event
   loop, which is where chrome, motion and input all land. → ADR-007.

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
A contract that several consumers implement should carry only what is used; speculative methods
are how an interface becomes something people satisfy without knowing why.

**`release_before` on `ChangeSource`** — removed after the fact, by its only consumer. It assumed
the terminal would retain a delta history that subscribers acknowledged their way through. The
encoder instead keeps a shadow of what each subscriber last saw and diffs against it, so there is
no shared history and nothing to release. A memory-management method that manages nothing tells
every caller memory *is* being managed, which is precisely the belief that stops someone checking.
The property it protected now holds by construction: per-subscriber state disappears with the
subscriber.

---

## Changing one anyway

1. Write down what breaks and why the current shape cannot serve — in the PR or
   its issue, so the reasoning outlives the diff.
2. Name every consumer — the table above is the list, and it must be *complete*
   before you start, not discovered while compiling.
3. Land the change and every consumer in one PR. A frozen contract with a
   half-updated consumer is worse than either shape.
4. Update the table in the same PR. A row that no longer describes the code is
   worse than no row, because it is believed.

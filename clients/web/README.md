# The web client

A browser tab is a client of a host's daemon exactly as the desktop window is,
speaking the same `zest-proto` messages. → [ADR-004](../../docs/ARCHITECTURE.md),
[docs/ROADMAP.md](../../docs/ROADMAP.md).

## What is here

```
packages/proto/    the wire protocol: framing, MessagePack en/decode, the delta
                   decoder, blocks — zero runtime deps, byte-golden to the Rust
packages/auth/     the Ed25519 handshake: transcript, pairing code, sign/verify
                   (@noble/ed25519 — the crypto is quarantined here), plus the
                   ClientSigner seam and, at ./webcrypto, the non-extractable
                   CryptoKey implementation of it
packages/client/   the data-plane session client: handshake driver, ack
                   cadence, resync, reconnect — remote.rs's lessons, ported
packages/input/    key/paste/focus → terminal bytes, a port of zest-input
packages/theme/    the 24 UiTokens, builtins, --zt-* CSS vars, the terminal
                   palette — zero runtime deps
packages/settings/ the settings schema and its walked UI fields, generated
                   from zest-config — zero runtime deps
packages/render/   the Canvas 2D grid painter, (grid, dirtyRows) → paint
packages/control/  the control-plane actors (SessionDirectory) — @sigx/actors
packages/sidecar/  the Node process hosting them: daemon feed in, actors
                   socket out, static files for the app
packages/app/      the sigx web app: session list, terminal view, input
```

Dependency policy: `proto`, `theme`, `input` and `settings` stay
dependency-free; crypto lives only in `auth`; sigx packages appear only in
`control`/`sidecar`/`app`.

**Two entry points in `auth`, and the split is deliberate.** `@zesterm/auth`
compiles under `lib: ES2023` alone, so `client` and `sidecar` stay
platform-blind — `client` says so in its own module doc, and `sidecar` is a
Node process. `@zesterm/auth/webcrypto` needs the DOM's `CryptoKey`, so only
the browser app imports it. Everything above the seam takes a `ClientSigner`
and cannot tell which kind of key it holds, which is the point:
`@noble/ed25519` **cannot** sign with a non-extractable key — it needs the raw
scalar, and never getting one is what the flag is for — so that path signs
through `crypto.subtle` and is asynchronous. Verification stays synchronous on
`@noble`, because the host must prove itself *before* the client signs
anything and an async verify makes the two interleavable.

**Generated, not written.** `packages/settings/generated/` and
`packages/theme/src/builtin.generated.ts` come out of `cargo xtask export-web`
and are gated by `cargo xtask check-export-web`. Edit the Rust and rerun it;
editing them directly is undone by the next person who does. The themes in
particular used to be hand-copied hex, and the transcription had no gate —
drift meant the native window and this client disagreed about what `obsidian`
looks like, with nothing to catch it.

## Where this runs

Two servers can serve this build, and it asks which at runtime — `GET
/api/bootstrap` returns `{"mode":"local"}` from the sidecar and
`{"mode":"cloud"}` from the Worker in `cloud/`. One `vite build` therefore
serves both; a `VITE_*` variable would have made them different bundles.

**The hosted client cannot use the LAN, ever.** An `https://` page may not open
`ws://192.168.1.5:7718` — mixed content, no workaround short of a certificate
on every daemon. So the deployed app routes every session through the relay,
including to the machine you are sitting at, and until that relay exists it
renders a card saying so rather than a session list that spins forever.

**The deployed app has no sidecar**, so there is nowhere at the edge to host
the control-plane actors — the hosted session list has to live in the tab,
written by one connection per host rather than read from a control plane. The
seam that lets one `SessionList` read both is
`packages/app/src/directory-source.ts`, which also records the two ways of
giving the hosted client an actor that were rejected. Only the actor-backed
implementation exists so far.

That makes the sidecar path **not** a subset of the cloud path: it is the one
that survives Cloudflare being unreachable, which ADR-005 requires of a local
terminal.

## Running the experience

```
zest-daemon --listen-ws                        # the data plane (port 7718)
pnpm --filter @zesterm/app build               # once, or after app changes
pnpm --filter @zesterm/sidecar start -- --static ../app/dist
open http://127.0.0.1:7350
```

`../app/dist`, not `packages/app/dist`: `pnpm --filter` runs the script from
the *package* directory, so a workspace-relative path resolves inside
`packages/sidecar/` and every file 404s while `/api/bootstrap` keeps working —
which reads as a broken build rather than a wrong flag.

Dev loop for the app itself: `pnpm --filter @zesterm/app dev` (vite, port
5173), with the sidecar started as
`--allow-origin http://localhost:5173` so the proxied actors socket passes the
origin posture.

## The one rule

**A client never interprets VT.** The host has already parsed it; what crosses
the wire is grid state. There is no terminal emulator in here and there must
never be one — two VT emulators means two truths, and desktop and phone would
drift on `wcwidth`, grapheme clustering and `DECSTBM` edge cases in ways nobody
can trace. Clients never parsing escape sequences also structurally eliminates
escape-sequence injection into a client parser.

Three rules that follow from it, all of which are in the types and all of which
are easy to get wrong:

- **Never recompute cell widths.** `Run.cells` is the host's decision. And never
  `text.length` — that counts UTF-16 code units, so one emoji counts as two and
  the row shifts. `[...text]` iterates code points.
- **Apply `scroll` before `row`** within a delta. The encoder emits them in that
  order and asserts it; a decoder that sorts writes rows into positions the
  scroll is about to overwrite.
- **Colours stay unresolved.** `Indexed(4)` renders against *this* client's
  theme, which is what lets a desktop and a phone show one session under
  different themes at the same time.

## Running it

```
pnpm install
pnpm -r typecheck     # also where the generated-bindings check fails
pnpm -r test
```

Node 24 or newer: the suites run TypeScript directly through Node's type
stripping, so there is no build step anywhere except the app (vite, because a
browser needs a bundle). `erasableSyntaxOnly` makes anything that would need a
transform a compile error rather than a surprise at runtime — the app package
alone opts out, for JSX.

## How it is checked

Against `crates/zest-proto/fixtures/`, generated by `cargo xtask fixtures` and
gated by `cargo xtask check-fixtures`. Each frame carries the complete framed
bytes *and*, independently, what the host terminal held after applying them —
read off a real `zest_core::Terminal`, **never off the Rust decoder**. Two
implementations agreeing with each other is not the goal; both agreeing with the
terminal is.

Two gates, catching different things:

| | Catches | Misses |
|---|---|---|
| `pnpm -r typecheck` | a wire *shape* that moved | anything about behaviour |
| `pnpm -r test` | applying the right shapes wrongly | a shape nobody decodes yet |

## Things worth knowing before changing anything

- **Wire integers are plain `number`s.** `rmp_serde` writes the narrowest
  encoding that fits, so `Seq`, `SessionId` and the line ids arrive as ordinary
  MessagePack integers, and since #14 the bindings say `number` to match. The
  one id outside ±2^53 a host actually sends — the `i64::MIN` a blank row is
  padded with — is a power of two and converts exactly; anything else that big
  is refused by `wire.ts` rather than rounded.
- **`Color` is generated, `color.ts` is the reader.** Since #15 `zest-core`
  has a `ts` feature and `AttrDef.fg`/`bg` import a real `Color.ts`;
  `bindings-match.test.ts` pins `color.ts`'s hand-written type to it, and the
  fixtures still exercise the parsing, because real sessions carry all
  three of its shapes.
- **The bit tables are hand-written too**, and the recordings *cannot* check
  them: they compare `flags` as raw integers, so a table mapping `ITALIC` to the
  wrong bit passes every frame. `fixtures/bits.json` is exported from the Rust
  `bitflags` definitions for that one purpose.
- **MessagePack is hand-rolled**, covering only what the daemon emits. If a type
  outside that subset ever appears — `ext`, timestamps, `bin` — swap `msgpack.ts`
  for `@msgpack/msgpack`; nothing outside that file would know.
- **`GridView` grows when a row lands past the end**; the Rust `Applier` asks for
  a keyframe instead. This is a port of `GridView`, and following the wrong one
  is a divergence no test outside the fixtures would catch.
- **A guessed echo is never in `GridView`.** `SessionClient.predictor` (the
  port of `zest-proto::predict`, ADR-016) guesses what a typed printable will
  echo as; `input(bytes, key)` takes the key *beside* the bytes because the
  predictor never un-encodes them, and the guess is drawn as a `predicted`
  span on the DOM prompt row — `paneModel`'s prompt item carries it. The
  canvas painter needs nothing: the alternate screen is never guessed into.
  Both ports replay `crates/zest-proto/fixtures/predict.json`; a rule that
  changes changes that file. `#442`.

## Not here yet

- **A Worker for the decoder.** Decode + apply runs on the main thread, and
  measurement says that is fine: the whole 82k-cell corpus replays in well under
  a second, and `fillText` — which must be on the main thread anyway — is where
  frame time actually goes. The `Dial → SessionClient → paint(grid, dirty)`
  seams make moving decode into a Worker mechanical if a profile ever demands
  it; do it on measurement, not on instinct.
- Selection/copy, mouse reporting, scrollback paging, splits, the palette —
  each named in the roadmap with its seam already in place. (IME composition
  is *not* on this list anymore: composed text — IME commits, the emoji
  picker, dictation — rides a hidden textarea into `encodeComposedText`.)

# The phone client — design

The Lynx app for M5, designed against what now exists: the daemon's WebSocket
transport, the sidecar's `SessionDirectory` actor, and the platform-blind
TypeScript packages the web client just proved end to end. This is a design
document, not a build; where it commits the codebase to something, it says so.

**The one-line shape:** the phone is the web client with a different chrome —
same two planes, same packages, blocks-first instead of grid-first.

## What the phone reuses, unchanged — a hard requirement

`@zesterm/proto`, `@zesterm/auth`, `@zesterm/client`, `@zesterm/theme` and the
blocks side of `@zesterm/control` are consumed **as they are**. This is the
payoff of two decisions already landed, and it is load-bearing enough to state
as a constraint on those packages rather than a hope about this one:

- **The `Dial` seam.** `SessionClient` and `ConnectionClient` take a
  `(handlers) => ByteLink` function and nothing else platform-shaped. The
  browser passed a `WebSocket`; the sidecar passed a `net.Socket`; the phone
  passes `@sigx/lynx-websocket`. A change to those packages that assumes a DOM,
  Node, or any global is a regression against this document.
- **Dependency hygiene.** `proto` and `theme` have zero runtime dependencies
  and `auth` only `@noble/*` (pure JS, no wasm) — nothing in the stack below
  the UI can fail to load on a phone runtime.

The conformance corpus applies as-is: the packages the phone runs are the ones
`pnpm -r test` already replays 82k cells and the block index through.

## Structure

A Lynx app (`~/dev/sigx/lynx`, install from npm) — the packages it leans on,
verified against the 0.26 package list:

| Package | For |
|---|---|
| `lynx`, `lynx-navigation` | the app, and the mockup's tab bar (screen 9): Sessions / Hosts / Blocks |
| `lynx-list` | the session list and the block feed (both are lists first) |
| `lynx-websocket` | both planes' sockets |
| `lynx-secure-storage` | the device's Ed25519 seed — see identity below |
| `lynx-keyboard` | the key-cap bar: `esc tab ctrl ↑ ⌃ ⌥ → / - \|` + `❯ run…` (screen 10) |
| `lynx-haptics` | long-press re-run confirmation |
| `lynx-safe-area`, `lynx-appearance` | the usual phone chrome |

Theme: `@zesterm/theme`'s tokens drive Lynx styles the same way they drive
`--zt-*` custom properties — `cssVarsOf` is a plain record, and the mapping
layer is the phone's, not the theme's.

## The two planes, on a phone

**Control** — `configureActors(socketTransport({ connect }))` from
`@sigx/actors-ws/client`, where `connect` adapts `lynx-websocket` to the
`SocketHandlers` seam (`onOpen`/`onMessage`/`onClose` — the same five-line
adapter the sidecar's own test writes around Node's `ws`). The session list is
the identical live read the web app uses:
`useActorState(SessionDirectory, key, 'list', { live: true })`. Nothing new is
designed here because nothing new is needed — that was the point of putting the
directory behind an actor.

**Data** — `SessionClient` over a `Dial` built on `lynx-websocket`, dialling
the `dataPlane` address the directory carries. Reconnect, ack cadence, resync
and drop-input-while-disconnected all come from the package; a phone that
sleeps and wakes is exactly the client `remote.rs`'s lessons were about.

One caveat to carry into implementation: a backgrounded phone app loses its
sockets. The `SessionClient` already treats that as an ordinary drop (redial,
re-attach, keyframe resync); the app must simply not fight the OS — reconnect
on foreground, and let the directory's `connected` flag drive the banner.

## Blocks-first, and when the grid appears

The phone renders **the block index, not the grid** (mockup screen 10). The
truth for that view is `GridView.blocks` — populated by the same keyframes and
deltas the desktop applies, never by re-parsing anything (ADR-004), and now
covered per-frame by `expect.blocks` in the conformance fixtures.

- **Finished blocks** render as cards: command, cwd, state rail, duration from
  `started_ms`/`ended_ms`. `exit_code: null` renders neutral — the wire type's
  own warning, already honoured by the web app's block rail.
- **The running block** streams its output rows inline: the rows between
  `output_line` and the grid's end, read from `rows`/`scrollback` by absolute
  line id — which is exactly what line ids are for.
- **Grid view is entered on `Modes.ALT_SCREEN`** (and offered, not forced, via
  `SessionInfo.alt_screen` in the list). The mode bit exists precisely so a
  client can switch representations structurally.

Rendering the grid on Lynx is the one genuinely open piece: Lynx (0.26) has no
canvas package. Three options, in preference order, decided when M5 starts:

1. **Native text rows** — a `lynx-list` of styled text runs built from
   `expandRow`. Terminal-correct monospace metrics are the risk.
2. **A webview** (`lynx-webview`) hosting `@zesterm/render`'s painter — reuses
   the proven renderer at the cost of a webview boundary for input.
3. A Lynx canvas API, if one lands before M5.

Blocks-first makes this genuinely deferrable: the alt-screen grid is the
*exception* view on a phone, not the product.

## Interactions

- **Tap** a block: expand/collapse its output (`foldedBlocks` is client-side
  state, per the mockup's list).
- **Long-press** a finished block: re-run — sends `Input` of `command + '\r'`
  over the data plane, with a haptic and a confirm. The client never
  re-evaluates anything; it replays keystrokes, and the shell decides.
- **Sticky Ctrl** in the key bar: latches, modifies the next key through
  `@zesterm/input`'s `mods`, releases. Same for ⌥.
- **`❯ run…`**: a text field whose submit is `Input` of the line + `'\r'` —
  the composed-text path that sidesteps per-key encoding entirely, which on a
  phone (IME world) is the *primary* input path, not the fallback.
- Rows ≥ 44px, per the mockup's measurements.

## Identity and auth — where the phone is ahead of the web

The phone keeps a **persistent** device key from day one:
`lynx-secure-storage` holds the 32-byte seed, `@zesterm/auth` derives and
signs. This is the posture the web client's localStorage seed is a stopgap
for, and it makes the phone the first client with a real device identity.

- First contact with a host: the six-digit `pairingCode` ritual, exactly as
  `attach --ws` and the browser do it today. The code is derived from the
  transcript both sides sign, so a relay shows two different codes — the
  property the whole flow exists for.
- The transcript and preimage are already phone-compatible **by design**:
  `identity.rs` chose prefix-then-plain-Ed25519 over Ed25519ctx explicitly so
  `@noble/ed25519` — which `@zesterm/auth` runs — can verify everywhere.
- M4's enrollment and 30-second single-use attach tickets slot in behind the
  same `Purpose` domains (`enrollment`, `attach-ticket`), reserved in the wire
  today. E2E encryption (Noise IK) is M5's follow-on, on the roadmap where it
  belongs.

## What this doc deliberately does not decide

Scrollback paging UX, multi-host tabs (the Hosts tab exists in the mockup; the
directory actor's key becomes the `HostId` when the fleet arrives — the shape
is ready), notifications for finished blocks, and the grid-rendering choice
above. Each has a seam waiting and none blocks starting.

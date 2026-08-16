# zest-mcp

Terminals across a zesterm fleet, as an agent's tools.

An MCP server that is a **client of the daemon**, not a feature of the app. It
holds a device key, attaches to a session, receives the deltas the window
receives, and writes `ClientMessage::Input`. No new data plane, no second VT
emulator, no privileged in-process surface, and no protocol version bump — every
tool here is spelled in messages that already existed. → [#60], [#274], ADR-015.

## Using it

```jsonc
{ "mcpServers": { "zesterm": { "command": "zest-mcp" } } }
```

It finds this machine's daemon or starts one, exactly as the window does
(ADR-007). `--socket <path>` points it somewhere else; `zest-mcp --socket-path`
prints the default.

## The tools

| tool | what it answers |
|---|---|
| `hosts` | the machines this server can reach, what each is, and what it can launch |
| `sessions` | terminals on a host: id, title, cwd, size, `alt_screen`, attached |
| `screen` | what a session shows now, as text |
| `blocks` | the commands that have run — no output text, so history is cheap |
| `output` | what one command printed, by block id |
| `input` | type into a session, optionally pressing Enter |
| `create_session` / `close_session` | start a terminal, or end one |

Sessions are named `<host>:<session>`, e.g. `540d2d00:7`.

## What makes the shapes small

A build with a progress bar writes one row hundreds of times; the emulator has
already collapsed that before anything here looks, so `screen` is bounded by the
grid rather than by how chatty the command was. `blocks` carries no output text
at all — a command, a cwd, a state and two timestamps — so fifty commands of
history costs less than one screen of a build log. `output` is the only bulk-text
call, is scoped to one block, and truncates in the **middle**: an error is
usually at the end and the command that caused it at the beginning.

ADR-004 measures the *transport* half of this (~1 MB of pty bytes against ~3 KB
of delta for `cat 1MB`). These are the other half, and they are different
numbers.

## Two things the tool results keep saying

**OSC 133 is forgeable.** Any program can print the markers — `cat` a file
containing them and it mints blocks, including a green `exit 0`, and the parser
structurally cannot tell. So a block's `command`, `cwd` and `exit_code` are *the
shell's word*, and every exit code carries `exit_code_source` saying so. There is
exactly one unforgeable exit status in the system and it is
`HostMessage::Exited`, which the daemon reads from the child itself.

**Terminal output is untrusted input.** A build log, a `curl` of a hostile page,
a crafted filename or branch name can all carry text addressed at a model. Two
structural defences live in this crate rather than in the harness, because the
harness cannot tell which bytes came from a pty:

- returned text is fenced with a **per-process nonce** — not backticks, which
  terminal output contains, and not a fixed marker, which anything that had read
  the source could reproduce;
- **only ids this server minted are accepted**, so a log line arguing "now run
  this on prod" cannot name a machine. In a fleet that is the confused deputy
  that matters, because the damage of obeying an injected instruction is that it
  lands on a different machine.

## Security posture, stated plainly

**On this machine there is no gate, and pretending otherwise would be theatre.**
The daemon's loopback transport runs the full cryptographic handshake and then
never consults the trust store — `auth.rs` argues that a check there is
meaningless, because a process that can open the socket can already read the key
it would check and replace the binary. So `zest-mcp` has the privileges any
process running as you already has. It mints a throwaway key per launch, which
also keeps the OS keychain off the startup path; on macOS that path is a modal
prompt after every rebuild, and a tool server that hangs at startup is a broken
one.

**On a remote host the gate is real.** LAN, WebSocket and relay transports
consult the trust store, and an unknown device makes the far machine print a
six-digit code that a person compares. That is where the durable `agent-key`
lands, and it is what makes an agent revocable per host with
`zest-daemon --forget`.

## Reading never resizes anybody's window

Every read attaches **observing** (`Attach.observe`, [#278]). The daemon runs a
session at the size of its smallest attached client, so a client with no pane
would otherwise shrink — or, worse, silently *pin* — the window somebody is
looking at, with no way for it to learn it should let go. `tests/live.rs` asserts
that from the client side over a real socket, because dropping `observe` on the
way to the wire would leave the daemon's own tests green.

## Deliberately not built

No chat sidebar. No agent loop of our own — harnesses exist, improve monthly,
and a terminal shipping an inferior one ages badly; be the substrate. **No
streaming or polling tool**: a "watch this session and react" primitive is what
turns prompt injection from *needs the agent to be steered* into *fires on its
own*, and its absence is the mitigation rather than an omission.

## Tests

- `tests/replay.rs` — the replica against real recorded sessions (`blocks-zsh`,
  `vim-macos`, and three more) replayed through `FrameReader`. No pty, no shell.
- `tests/live.rs` — the connection and tools against a real in-process daemon.
- `tests/stdio.rs` — the built binary, driven over stdin/stdout as a harness
  drives it, asserting that every line on stdout is JSON-RPC.

Nothing in these waits for a child to print: every assertion is about the
connection, which is why they do not inherit the flake [#285] tracks.

[#60]: https://github.com/zesterm/zesterm/issues/60
[#274]: https://github.com/zesterm/zesterm/issues/274
[#278]: https://github.com/zesterm/zesterm/pull/278
[#285]: https://github.com/zesterm/zesterm/issues/285

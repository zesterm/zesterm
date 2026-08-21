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
| `screen` | what a session shows now, as text — with `styled` saying where it is dim or reversed |
| `blocks` | the commands that have run — no output text, so history is cheap |
| `output` | what one command printed, by block id |
| `run` | run a command in an existing shell and wait for the shell to say it ended |
| `run_isolated` | run a command in a terminal of its own, for the **unforgeable** exit code |
| `input` / `interrupt` | type into a session, send it named keys or a paste, or send it Ctrl+C |
| `create_session` / `close_session` | start a terminal, or end one |

Sessions are named `<host>:<session>`, e.g. `540d2d00:7`.

## `run`, and why it is the primitive

Agent harnesses cannot tell when a command finished in an interactive shell, so
they inject a sentinel — `echo __done_$?` — and read it back. That cannot
distinguish a command that ended from one sitting at `Password:`, and it does not
survive the user's own shell being a real one.

The shell already says. `run` writes the line, then waits for the OSC 133 `D` the
shell emits when the command ends, parsed host-side into a block with its own
exit code — in the user's *interactive* shell, so a virtualenv, an ssh-agent and
a kubectl context are all where they were. **A timeout does not kill**: the block
comes back `running` with its partial output and the session is left alone, so
the `Password:` case can be answered with `input`, stopped with `interrupt`, or
followed with `blocks` and `wait: true`.

`run_isolated` is the other half. It needs no shell integration — bash, fish and
cmd.exe emit no markers, which is most Linux hosts rather than an edge case — and
its status comes from the process rather than from a marker, so it is the one
exit code here that nothing running inside the terminal can forge.

**The anchor is the tail block, not the next id** — the same rule `blocks(wait:)`
uses, and `run` calls `block_anchor`/`finished_since` rather than keeping a second
copy of it. `src/run.rs` adds only what *writing* needs on top: the states a wait
does not care about (a command the shell never started, a block a screen clear
destroyed), and the refusals a wait does not need (an alt screen, a shell with no
markers, a command already running, and the gap between `D` and the next prompt).

## Keys have names, because an agent has no keyboard

Every other client encodes a keystroke at the keyboard that produced it —
`zest-input` from a `winit::KeyEvent`, the browser from a `KeyboardEvent` —
because modifier conventions belong to the platform. There is no such platform
here, so `input` takes **named keys** and encodes them on this side:
`keys: ["down", "down", "enter"]`, with `ctrl+`, `alt+` and `shift+` prefixes.

That is not decoration. An arrow is `ESC [ A` or `ESC O A` depending on DECCKM,
which lives on the *host* — so a model writing the sequence into a JSON string is
right about half the time and silently does nothing the rest. Measured: roughly
**2 attempts in 10** reached the application, the remainder arriving as literal
text in a composer (`❯ [Z[Z[Z`) or as nothing at all. An unknown name is refused
with the vocabulary rather than ignored, because a key that quietly does nothing
is indistinguishable from one the application chose not to handle.

**Each part is its own keystroke.** `text`, then `paste`, then each key, then
`submit`'s Enter — one `ClientMessage::Input` each, which is one `write` on the
pty. `submit` used to share a write with the text, and a TUI that tells a
keystroke from a paste on exactly that boundary took the whole thing as pasted:
the CR became a newline in the composer and nothing submitted, so every message
cost two round trips ([#344]).

Splitting the write is necessary and **not sufficient** — a tty hands the next
raw-mode `read()` everything queued, and on Windows conhost parses the pipe into
input records on its own schedule. What closes it is `paste`, a separate argument
that wraps text in the bracketed-paste markers when the program asked for them,
exactly as a real terminal does. Separate, and never inferred from `text`: DEC
2004 is set for a program's whole run, not for the moments a paste would be right
— `nvim` has it on in normal mode, so auto-wrapping `text: ":wq"` would insert it
into the buffer instead of executing it, with nothing to see. The web client
already draws this line, in `paste.ts` and `text.ts`.

The Enter is never *inside* the markers: every shell inserts a bracketed paste
into the line buffer without running it, so a CR within the brackets is inserted
literally — which is [#344] again wearing a different hat. Outside them it
executes, exactly as it does for a person who pastes and then presses Enter.

Only the six DECCKM-sensitive keys and a paste need the session's modes, and
those are reachable only from a replica — so ordinary typing still costs no
attach, as it always has.

**This is the third copy of one table.** `crates/zest-input/src/key.rs` is the
source of truth and `clients/web/packages/input/src/key.ts` is a case-for-case
port of it; `src/keys.rs` exists only because `zest-input` takes `winit` types in
its public API. Three copies of one rule is what gave `Grid::drop_scrollback_rows`
three semantics, so this one is not held by review: `tests/keys.rs` encodes every
name both ways — 27 bases × 8 modifier combinations × DECCKM on and off — and
byte-inequality fails the build. `zest_input::key::encode_press` exists for it,
because `KeyEvent` has a private platform tail and cannot be built outside winit.

## Dim text is not typed text

Flattened to characters, text an application is *offering* is identical to text
the user has **committed**. A CLI's greyed suggestion and a real command line
have the same `>` and the same letters; rendered, a human sees one is a ghost.
Through `screen` they were indistinguishable — and an Enter sent for any other
reason **accepts** the suggestion. The one that prompted [#348] read "go ahead,
branch and open the issue", one keystroke from real work in the repo.

The same flattening loses a picker's *selection* whenever it is drawn by
inverting the row rather than by printing a marker, which leaves nothing at all
to read the cursor position from — so an arrows-only dialog could be navigated
but not aimed.

So `screen` carries `styled`: `{row, col, len, attrs}`, where `row` indexes the
returned lines and `col`/`len` are grid columns, the units `cursor` already uses.

Three choices in that shape are worth stating:

- **Positions, never text.** Returning attributed *runs* would restate the whole
  screen a second time, JSON-escaped — 3-5x the tokens of the plain text on a
  realistic TUI frame, 20x+ on a syntax-highlighted one. Spans measured 2-23
  bytes across the recorded corpus. It also means the value contains no
  characters a terminal produced, so it needs no untrusted fence of its own:
  there is nothing in it a hostile program could author.
- **Always, not on request.** A safety signal behind an opt-in flag is absent
  exactly when it was needed, because the caller who would set the flag is the
  one who already suspected. The key is omitted entirely when a screen carries
  no attributes, which is the common case for a shell at a prompt.
- **No colour.** `fg`/`bg` are where nearly all the run-splitting lives, and
  neither case above needs them: dim is its own bit and reverse is its own bit.
  A red error line is still just text. The three *layout* bits share the same
  word — `WIDE`, `WIDE_SPACER`, `WRAPLINE` — and are masked out, because 250 of
  274 flagged runs in the `vim-macos` recording are `WRAPLINE` alone.

**A blind spot, stated.** No recording in the corpus contains reverse video at
all, so the selection case cannot be replayed from a fixture; it is covered by
VT-driven tests in `src/session.rs` instead. Same shape as [#17]. And an
application that fakes selection with explicit colours rather than SGR 7 sets no
`INVERSE` bit and will not appear here.

`changed_since` ([#319] item 3) was decided alongside this and deliberately not
built: spans live in a sibling key keyed by row, so a future row filter composes
with them rather than changing their shape.

## What makes the shapes small

A build with a progress bar writes one row hundreds of times; the emulator has
already collapsed that before anything here looks, so `screen` is bounded by the
grid rather than by how chatty the command was. `blocks` carries no output text
at all — a command, a cwd, a state, where it sits in the session's lines, and
two timestamps — so fifty commands of history costs less than one screen of a
build log. `output` is the only bulk-text call, is scoped to one block, and
truncates in the **middle**: an error is usually at the end and the command that
caused it at the beginning.

ADR-004 measures the *transport* half of this (~1 MB of pty bytes against ~3 KB
of delta for `cat 1MB`). These are the other half, and `examples/token_probe.rs`
measures both on a command of your choosing:

```sh
cargo run -p zest-mcp --example token_probe -- --cmd "cargo build"
cargo run -p zest-mcp --example token_probe -- --run "ls -la"   # via a shell, so `output` has blocks
```

It spawns rather than replaying a fixture, because the recorded corpus has no
build in it — the largest entry is 10 KB of `vim` — and committing build logs
would pay storage forever for a number that moves with the toolchain. The
`screen` and `output` figures come from a real `Replica` fed the encoder's own
output, so they are what a tool returns rather than a second reading of the grid.

**The two numbers behave differently, and that is the point.** `seq 1 200000` is
1.49 MB of pty — roughly 596k tokens if something scraped the stream — and
reaches a model as **202 bytes, about 51 tokens**. That figure does not move,
because `screen` is the final grid: it is bounded by the grid rather than by how
much was printed, so it gets *better* the noisier the command is.

The transport figure is not a property of the session at all. Deltas coalesce on
**state**, so the same run costs 3,254 bytes if you poll once, 507 KB on a 16 ms
frame, and 11.4 MB if you ask after every read — larger than the stream it
replaces. The first of those reproduces ADR-004's ~3 KB almost exactly, which
settles what that number is: the single-delta floor, not a saving every client
receives. Quote them separately.

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

- returned text is fenced with a **nonce minted per call** — not backticks, which
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
and a terminal shipping an inferior one ages badly; be the substrate. **Nothing
that delivers output with no call outstanding**: a "watch this session and
react" primitive is what turns prompt injection from *needs the agent to be
steered* into *fires on its own*, and its absence is the mitigation rather than
an omission.

The line is at the call, not at the waiting. `screen` and `blocks` both block
until something happens (below), because a wait cannot manufacture a turn — the
agent asked, the answer is that call's result, and nothing runs afterwards
unless the harness grants another one. What stays unbuilt is anything that
speaks when nothing asked. ADR-015, amended in #319.

## Waiting instead of polling

`screen` answers immediately unless it is given `after_seq`, and then it returns
the moment the screen moves past that sequence — pass back the `seq` from the
previous read. Add `idle_ms` and it goes further: after the screen moves, it
keeps waiting until the output has *stopped*, which is the difference between
"tell me when the build starts printing" and "tell me when it has finished".
`blocks` takes `wait: true` and returns when a command ends.

Both are bounded by `timeout_ms` under the same ceiling as everything else a
model can spend, and **a deadline passing is a result, not an error**: the
screen (or the block list) comes back with `timed_out: true`, the way
`run_isolated` returns partial output rather than failing. A wait also ends when
the session's child does, so watching a shell that has died costs nothing rather
than the whole deadline.

The one part that is not obvious: `blocks(wait:)` reports `finished_block`, and
it is routinely a block the caller has **already seen**. OSC 133;C makes the
shell reuse its trailing prompt block for the command typed at it, so the thing
that just finished usually has an id at or below the highest one already
listed — which is why the wait anchors on the tail block rather than on
`since_id`, and why `finished_block` is named separately instead of the caller
being told to look for something new. `tests/replay.rs` holds that against a
real zsh recording, and measures the naive predicate beside it.

## Tests

- `src/run.rs` — the correlation as a pure function: every refusal, every id
  case, nothing spawned. `src/tools.rs`'s `Plan` is the same idea for `input`:
  how many writes and in what order, with no daemon. The [#344] regression test
  lives there rather than in `tests/live.rs` because a message boundary is not
  observable from a client watching a real pty — a live test could not have
  caught the bug it fixes.
- `tests/keys.rs` — the named-key table against `zest-input`, byte for byte.
- `tests/replay.rs` — the replica against real recorded sessions (`blocks-zsh`,
  `vim-macos`, and three more) replayed through `FrameReader`. No pty, no shell.
  `run`'s correlation is held here too, against a genuine zsh session recorded
  off a real pty, which is what makes the shell-shaped half of this crate
  testable on every CI platform.
- `tests/live.rs` — the connection and tools against a real in-process daemon.
- `tests/stdio.rs` — the built binary, driven over stdin/stdout as a harness
  drives it, asserting that every line on stdout is JSON-RPC.

Almost nothing here waits for a child to print: every other assertion is about
the connection, which is why they do not inherit the flake [#285] tracks. The one
exception is `a_run_against_a_real_shell_returns_the_shells_own_exit_code`, which
cannot exist without a shell that emits the markers — it puts the shell's startup
on its own budget so a slow runner fails as a slow runner, and where neither zsh
nor PowerShell is installed it says so on stderr and returns rather than
asserting something weaker.

[#60]: https://github.com/zesterm/zesterm/issues/60
[#274]: https://github.com/zesterm/zesterm/issues/274
[#278]: https://github.com/zesterm/zesterm/pull/278
[#344]: https://github.com/zesterm/zesterm/issues/344
[#345]: https://github.com/zesterm/zesterm/issues/345
[#348]: https://github.com/zesterm/zesterm/issues/348
[#319]: https://github.com/zesterm/zesterm/issues/319
[#17]: https://github.com/zesterm/zesterm/issues/17
[#285]: https://github.com/zesterm/zesterm/issues/285

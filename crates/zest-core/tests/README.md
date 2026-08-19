# zest-core test fixtures

Two kinds, deliberately kept apart because they answer different questions.

## `ansi/*.ans` — synthetic spec fixtures

Raw byte streams, hand-constructed to exercise specific VT behavior: SGR
attributes, truecolor and 256-colour, cursor movement and scroll regions, OSC
(title, palette, hyperlinks, and the OSC 133 prompt markers that command blocks
will need), and the Unicode width edge cases — CJK, combining marks, and ZWJ
emoji sequences.

These are precise and deterministic. They say *"the parser handles this sequence
correctly"*, and they are the right place to test anything the spec defines.

## `corpus/*.vtrec` — recordings through a real ConPTY

Captured with `cargo run -p zest-pty --example pty_dump -- --record <file> --cmd <...>`,
these are what real programs actually emitted through a real pseudoconsole,
with timestamps.

These are messy and realistic. They say *"the parser survives what software in
the wild actually does"*, and they catch integration bugs that hand-written
sequences never will — because nobody hand-writes the sequence a program
actually produces.

### Format

Deliberately trivial, so a test can parse it in ten lines:

```
VTREC1\n
<micros:u64le><len:u32le><bytes>   (repeated)
```

### Record neutrally

`blocks-zsh.vtrec` and `blocks-pwsh.vtrec` are real interactive shells with
zesterm's shell integration injected — the recordings that make the command-block
assertions mean something, since the other five predate shell integration and
carry no OSC 133 at all.

It was captured with `HOME`, `HOST` and the working directory pointed at
throwaway values, and **that is the rule for anything committed here**: a
recording carries whatever the prompt printed, which is a username, a hostname
and a path. Every file in this directory is clean of all three, and a recording
that identifies whoever made it is one nobody else can re-record.

```
mkdir -p /tmp/zestdemo/home && echo 'PS1="%~ %# "' > /tmp/zestdemo/home/.zshrc
cd /tmp/zestdemo && HOST=zesterm-demo HOME=/tmp/zestdemo/home \
  ZDOTDIR=<config>/shell-integration/zsh ZESTERM_USER_ZDOTDIR=/tmp/zestdemo/home \
  pty_dump --cmd "/bin/zsh -i" --record blocks-zsh.vtrec --idle-exit-ms 2500 < commands.txt
```

`blocks-pwsh.vtrec` is the same thing on Windows: PowerShell 7 with the hook
dot-sourced by the command line `install` builds, run from `C:\zestdemo` and with
the default prompt, which prints the path and nothing else.

Two things about it are not obvious. **Do not pipe the commands in.** PSReadLine
reads whatever is already buffered as one paste, so the whole script arrives as a
single mangled command line; drive stdin with real pauses between lines and write
`\r` alone, since a stray `\n` ends up inside the *next* recorded command. And
**set the child's working directory explicitly** — `Set-Location` moves
PowerShell's `$PWD` but not the process's CWD, which is what the pty inherits, so
a recording made this way otherwise carries the directory the shell was launched
from rather than the one it appears to be in.

The prediction repaints PSReadLine does are in the recording on purpose: they are
why the hook states the command with `633;E` instead of leaving zesterm to read it
back off the grid.

### `resize-drag.vtrec` — a height drag through a real ConPTY

The gesture #247 is about: fill the screen, drag the height down, drag it back.
Recorded at **100x30**, shrunk to 100x8 and grown back:

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-drag.vtrec `
  --cmd "pwsh -NoLogo -c ls; Start-Sleep 6" `
  --size 100x30 --resize 100x8 --resize 100x30
```

Two resizes rather than one, because a drag is a shrink *and* a grow and they
answer differently — and it is the grow's repaint that `Grid::settle_restate`
turns on. A capture with one resize cannot reach the thing being tested, which
is why `--resize` is repeatable.

**The spawn geometry is part of the fixture and is not in the file.** A `.vtrec`
is timestamped bytes and nothing else, so the replay builds its grid at the size
the recording was made at: the output before the first resize was laid out for
that width, and a grid at another one wraps it somewhere ConPTY never did.
`pty_dump` logs the size it spawned at for this reason, and the replay test
hard-codes it beside the filename.

The neutrality rule above applies as much here as anywhere — `ls` in a directory
whose name is nobody's, with the default prompt.

### `resize-drag-storm.vtrec` — the same drag as a mouse makes it

The recording above lets each repaint finish before the next resize; a real drag
does not. winit fires resizes throughout the gesture, ConPTY coalesces its
answers, and the repaints that do arrive are laid out for sizes the grid has
already left — which is where #312 lived: an unannounced stale repaint's settle
destroyed the history it thought it was giving back. Recorded at **100x30**,
shrunk to 100x8, then four grows issued back-to-back:

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-drag-storm.vtrec `
  --cmd "pwsh -NoLogo -c `"ls; Start-Sleep 8`"" `
  --size 100x30 --resize-after-ms 1500 --resize-settle-ms 400 --resize 100x8 `
  --resize-after-ms 0 --resize-settle-ms 0 `
  --resize 100x14 --resize 100x20 --resize 100x26 --resize-settle-ms 2000 --resize 100x30
```

`--resize-settle-ms 0` is what makes it a storm: with no settle window the four
grows land within ~100µs of each other, faster than ConPTY's first answer
(~300µs on the box that recorded this). ConPTY then answers with *two* repaints
— one for an intermediate size it had already been resized past, one for the
final size — and only the very first repaint of the whole gesture announces
itself with `CSI 8 t`. The replay test asserts that shape and says to re-record
if a new capture loses it.

### `resize-drag-stepped.vtrec` — the same drag at the daemon's cadence

The other failure mode needs the *opposite* timing: a resize every 120ms, each
answered by a matching repaint before the next lands. Nothing stale anywhere —
and each intermediate settle still lost rows, because the next repaint restates
ConPTY's buffer, which never got the pulled rows back (#312's second half; the
provisional settle is the fix). Two `ls`es so the session has real history, at
**80x24**, stepped 20, 14, 8, 14, 20, 24:

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-drag-stepped.vtrec `
  --cmd "pwsh -NoLogo -c `"ls; ls; Start-Sleep 8`"" `
  --size 80x24 --resize-after-ms 2000 --resize-settle-ms 120 --resize 80x20 `
  --resize-after-ms 0 --resize 80x14 --resize 80x8 --resize 80x14 --resize 80x20 `
  --resize-settle-ms 2000 --resize 80x24
```

Its replay test builds the expected screen from the fixture itself — every
chunk except the drag window's — so a re-recording carries its own golden.

### `resize-drag-overflow.vtrec` — the shrink half arrives late and too tall

Three shrinks issued back-to-back (`--resize-settle-ms 0`), a 300ms turnaround,
four grows back-to-back. ConPTY's first answer is then laid out for 24 rows and
parses into a grid already at 8 — sixteen rows of overflow, which is the #315
mechanism: each overflow scroll used to cancel the restate debt and bank a
duplicate of a row the grid already held. At **100x30**:

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-drag-overflow.vtrec `
  --cmd "pwsh -NoLogo -c `"ls; Start-Sleep 8`"" `
  --size 100x30 --resize-after-ms 2000 --resize-settle-ms 0 --resize 100x24 `
  --resize-after-ms 0 --resize 100x16 --resize 100x8 `
  --resize-after-ms 300 --resize 100x14 --resize-after-ms 0 --resize 100x20 `
  --resize 100x26 --resize-settle-ms 2000 --resize 100x30
```

One capture, both traps: the 24-row repaint overflows (#315), the 20-row one is
stale-smaller and refused by coverage (#312), and the 30-row one settles. The
replay uses the same fixture-derived golden as the stepped test.

### `resize-drag-thirdleg.vtrec` — down, up, and up again

The reported gesture's third leg (#335): shrink to 8, grow to 30 with time for
the settle to land, then shrink again — twice, partially — with ConPTY
answering each move. After a settle this grid's viewport permanently holds more
than ConPTY's buffer, so every later repaint restates a lesser truth; the
re-bank must survive the shrink that precedes it. Slow steps on purpose
(`--resize-settle-ms 1500`): this is the deliberate up–down–up a hand makes,
not a storm.

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-drag-thirdleg.vtrec `
  --cmd "pwsh -NoLogo -c `"ls; Start-Sleep 10`"" `
  --size 100x30 --resize-after-ms 2000 --resize-settle-ms 1500 --resize 100x8 `
  --resize-after-ms 0 --resize 100x30 --resize 100x28 --resize-settle-ms 2000 --resize 100x26
```

Its replay compares the multiset of non-blank line texts against a drag-free
replay of the same fixture: the layout may differ (some of the listing
legitimately lives in scrollback at 26 rows), the content may not.

### `resize-width.vtrec` — the width axis

Two `ls`es at 100x30, narrowed to 50 and widened back, slow steps (#224):

```powershell
cargo run -p zest-pty --example pty_dump -- `
  --record crates\zest-core\tests\corpus\resize-width.vtrec `
  --cmd "pwsh -NoLogo -c `"ls; ls; Start-Sleep 8`"" `
  --size 100x30 --resize-after-ms 2500 --resize-settle-ms 1500 --resize 50x30 `
  --resize-after-ms 0 --resize-settle-ms 2000 --resize 100x30
```

What it proves: ConPTY restates logical lines (the two reflows cannot disagree
about wrapping), the narrow halves tail-anchor identically, and the widen
repaint restates **from home** — opening, in this capture, by rewriting the
wrapped *fragment* its buffer's top row held (`ESC[H crates ESC[K`), which is
the corner the width anchor's fragment rule exists for. The replay asserts
nothing is destroyed or doubled, allowing only the restater's own fragment
rows as extras.

Worth adding as they become relevant: `vim`, `htop`/`btm`, `tmux`, a `cargo build`,
and the `@sigx/terminal` showcase example — the last being a useful check that
zesterm can host the user's own TUI framework correctly.

```
cargo run -p zest-pty --example pty_dump -- --record crates/zest-core/tests/corpus/vim.vtrec --cmd vim
```

Note that recording a *quoted* command through a shell mangles escape sequences
easily; if a recording comes back as plain text where you expected SGR, that is
almost certainly shell quoting rather than a terminal bug. Prefer a synthetic
`.ans` fixture when what you want is a specific escape sequence.

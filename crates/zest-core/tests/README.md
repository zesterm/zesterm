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

### Recording more

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

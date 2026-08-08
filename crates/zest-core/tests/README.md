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

`blocks-zsh.vtrec` is a real interactive `zsh` with zesterm's shell integration
injected — the recording that makes the command-block assertions mean something,
since the other five predate shell integration and carry no OSC 133 at all.

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

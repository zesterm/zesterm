# Claude Code guide (sigx standard)

@AGENTS.md

The imported `AGENTS.md` above is the canonical, tool-neutral guide — everything
shared lives there, including the **branch-first** warning at its top. Below are
only the Claude-Code-specific bits.

## Claude Code specifics

- **Worktrees**: Claude Code sessions are per-directory, so `pnpm wt new <name>`
  plus launching Claude Code from `<repo>/branches/<name>` gives a fully
  independent parallel session — no extra wiring needed. Budget for a cold
  `cargo build` in each new worktree; see the Rust note in `AGENTS.md`.
- **Bash vs PowerShell**: both tools are available and take their own syntax.
  Anything carrying a `\\.\pipe\…` path, or a path destined for another machine
  in the fleet, goes through PowerShell — Git Bash rewrites those arguments
  before the program sees them (see "Traps already paid for").

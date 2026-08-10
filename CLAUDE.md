# Claude Code guide (sigx standard)

@AGENTS.md

The imported `AGENTS.md` above is the canonical, tool-neutral guide — what
zesterm is, the workflow, the gates, the commands, the conventions, the traps
already paid for, and the git-worktree flow all live there. Below are only the
Claude-Code-specific bits.

## Claude Code specifics

- **Branch first — never work on `main`.** Before touching any file:
  `pnpm wt new <N-short-slug>`, then continue from
  `<repo>/branches/<N-short-slug>`. Verify with `git branch --show-current`
  before every commit; if it prints `main` or nothing (detached HEAD), stop —
  move the changes
  (`git stash -u` → `pnpm wt new <N-short-slug>` →
  `cd <repo>/branches/<N-short-slug>` → `git stash pop`) instead of
  committing. See the warning at the top of `AGENTS.md`.
- **Worktrees**: Claude Code sessions are per-directory, so `pnpm wt new <name>`
  plus launching Claude Code from `<repo>/branches/<name>` gives a fully
  independent parallel session — no extra wiring needed. Budget for a cold
  `cargo build` in each new worktree; see the Rust note in `AGENTS.md`.
- **Bash vs PowerShell**: both tools are available and take their own syntax.
  Anything carrying a `\\.\pipe\…` path, or a path destined for another machine
  in the fleet, goes through PowerShell — Git Bash rewrites those arguments
  before the program sees them (see "Traps already paid for").

# zesterm

A GPU-accelerated, themable terminal — built to be driven from anywhere.

Native on Windows, macOS, and Linux via `wgpu` (DX12 / Metal / Vulkan) and `winit`.
The end goal is to reach your machine's shells from a browser or a phone, so the
architecture separates a headless terminal core from any particular frontend from
the very first commit.

## Status

Milestone 1 (a local Windows terminal good enough to replace Windows Terminal) is in
progress. Everything upstream of the renderer works:

```
cargo run -p zest-app --example headless
```

spawns a shell, parses its output, and prints the resulting grid.

| Piece | State |
|---|---|
| `zest-pty` — ConPTY, resize, shutdown, `.vtrec` recording | working |
| `zest-core` — grid, scrollback, VT parsing, modes, OSC | working, 77 tests |
| `zest-font` — metrics, shaping, rasterization, fallback | working, 22 tests |
| `zest-theme` — tokens, OKLCH colour math, built-ins, importers | working, 44 tests |
| Transparency capability probe | done — see ADR-003 |
| `zest-render-wgpu` — atlas, 3 pipelines, offscreen resolve | renders offscreen, 14 tests |
| `zest-input`, `zest-config` | not started |

The font layer renders a sample sheet to a PNG with no GPU involved, which is
where font bugs are cheapest to find:

```
cargo run -p zest-font --example font_dump -- --size 24 --ligatures
```

The renderer does the same — a real terminal grid to a PNG, no window involved,
on a fallback adapter so it runs in CI:

```
cargo run -p zest-render-wgpu --example render_dump
```

## Layout

| Crate | Responsibility |
|---|---|
| `zest-core` | VT parsing, grid, scrollback. **No UI, no GPU, no process APIs.** |
| `zest-pty` | ConPTY / forkpty spawning and byte I/O |
| `zest-theme` | Token schema, perceptual color math, scheme importers |
| `zest-font` | Font discovery, shaping, CPU rasterization |
| `zest-render-wgpu` | Glyph atlas and the SDF / glyph / decoration pipelines |
| `zest-input` | Key and mouse events to terminal byte sequences |
| `zest-config` | Settings cascade, profiles, hot reload |
| `zest-app` | The `zesterm` binary: window, chrome, motion, wiring |

`zest-core` is the load-bearing boundary — it must never depend on `wgpu`, `winit`,
`windows`, or `tokio`, and must build for `wasm32-unknown-unknown`. This is what lets
the native app, the remote daemon, and the browser/mobile clients share one terminal
implementation instead of three that quietly diverge. Run `cargo xtask check-deps` to
verify; CI does the same.

## Building

Requires the MSVC toolchain on Windows — the Windows SDK alone is not enough, because
Rust's `libstd` needs the MSVC CRT (`__CxxFrameHandler3`, `__chkstk`) which only ships
with VC Tools:

```
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100"
```

Then:

```
cargo build --workspace
cargo xtask check-deps
```

## Theming

Theme `ui.*` tokens are [`@sigx/terminal-zero`](https://sigx.dev/terminal/docs/theming)'s
contract verbatim, so one theme file styles zesterm's chrome *and* any `@sigx/terminal-ui`
TUI running inside it. Importers are planned for iTerm2 `.itermcolors`, Windows Terminal
schemes, base16/base24, and Alacritty/Ghostty TOML.

## License

MIT OR Apache-2.0

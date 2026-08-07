# zesterm

A GPU-accelerated, themable terminal — built to be driven from anywhere.

Native on Windows, macOS, and Linux via `wgpu` (DX12 / Metal / Vulkan) and `winit`.
The end goal is to reach your machine's shells from a browser or a phone, so the
architecture separates a headless terminal core from any particular frontend from
the very first commit.

## Status

Early. Milestone 1 (a local Windows terminal good enough to replace Windows Terminal)
is in progress.

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

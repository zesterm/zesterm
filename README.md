# zesterm

A GPU-accelerated, themable terminal — built to be driven from anywhere.

Native on Windows, macOS, and Linux via `wgpu` (DX12 / Metal / Vulkan) and `winit`.
The end goal is to reach your machine's shells from a browser or a phone, so the
architecture separates a headless terminal core from any particular frontend from
the very first commit.

## Status

**It runs.** Build once, then run the binary:

```
cargo build --release
./target/release/zesterm
```

While working on it, use the `fast` profile instead — `release` uses thin LTO and
a single codegen unit, so a one-line change costs ~51s to rebuild versus ~3.6s:

```
cargo run --profile fast -p zest-app
```

A window with a real shell in it — GPU-rendered, themed, with working input,
scrollback, and selection.

Mouse: drag to select, double-click a word (paths and identifiers stay whole),
triple-click a line, Alt-drag for a rectangle. Ctrl+Shift+C / Ctrl+Shift+V copy
and paste; right-click copies when there is a selection and pastes otherwise;
middle-click pastes.

The window appears in **~50ms** and the shell prompt is on the first frame.

> Run the **binary**, not `cargo run`. Cargo re-resolves the workspace and
> freshness-checks every source file before it execs anything, which costs
> ~500ms on this workspace even when there is nothing to rebuild — comparable to
> zesterm's entire startup. Measured: `cargo run --release -p zest-app` ~560ms
> versus ~22ms direct, for a command that does nothing but print and exit.

Milestone 1 (good enough to replace Windows Terminal daily) is in progress; see
[docs/ROADMAP.md](docs/ROADMAP.md).

| Piece | State |
|---|---|
| `zest-pty` — ConPTY, resize, shutdown, `.vtrec` recording | working |
| `zest-core` — grid, scrollback, VT parsing, modes, OSC | working, 77 tests |
| `zest-font` — metrics, shaping, rasterization, fallback | working, 22 tests |
| `zest-theme` — tokens, OKLCH colour math, built-ins, importers | working, 44 tests |
| Transparency capability probe | done — see ADR-003 |
| `zest-render-wgpu` — atlas, 3 pipelines, offscreen resolve | renders offscreen, 14 tests |
| `zest-app` — window, threads, input, selection | working, 24 tests |
| `zest-config` | not started |

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

And the app itself renders one real frame — real padding, chrome, theme and scale
factor — to a PNG without ever showing a window, which needs no screen-recording
permission and works over SSH:

```
zesterm --screenshot shot.png [--screenshot-size 1200x800] [--screenshot-delay 400]
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

On macOS the Xcode Command Line Tools are the whole requirement — the linker and
SDK come with them, and nothing else needs installing:

```
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Use rustup rather than Homebrew's `rust` formula: only rustup honours the
`rust-toolchain.toml` pin, and a mismatched compiler against a workspace that
sets `rust-version` fails in ways that look like code errors.

Then, on either platform:

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

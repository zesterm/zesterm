# zesterm

A GPU-accelerated, themable terminal — built to be driven from anywhere.

Native on Windows, macOS, and Linux via `wgpu` (DX12 / Metal / Vulkan) and `winit`.
The end goal is to reach your machine's shells from a browser or a phone, so the
architecture separates a headless terminal core from any particular frontend from
the very first commit. What the system is — the fleet, the layers, the crate map —
is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)'s "The system" overview; current
state and open work are in [docs/ROADMAP.md](docs/ROADMAP.md).

## Status

**It runs, daily.** Build once, then run the binary:

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
scrollback, selection, and command blocks, attached to this machine's daemon so
the shell outlives the window.

Mouse: drag to select, double-click a word (paths and identifiers stay whole),
triple-click a line, Alt-drag for a rectangle. Ctrl+Shift+C / Ctrl+Shift+V copy
and paste; right-click copies when there is a selection and pastes otherwise;
middle-click pastes.

The window appears in **~35ms** on Windows (48ms on macOS) and the shell prompt
is on the first frame. Measure with the built binary, not `cargo run` — cargo's
workspace resolution costs ~500ms before the process starts (see `AGENTS.md`
§ Commands).

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

A Rust workspace under `crates/` (13 crates), plus `xtask/` for the repo's gates,
`clients/web/` for the browser client and `cloud/` for the Cloudflare Workers
that host it. The full crate map, one line each, is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) § The map.

`zest-core` is the load-bearing boundary: no `wgpu`/`winit`/`windows`/`tokio`,
builds for wasm — see `AGENTS.md` § The one invariant, and ADR-001 for why.
`cargo xtask check-deps` verifies it; CI does the same.

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

On Linux the build needs the development headers winit and fontique link
against — the same ones CI installs:

```
# Debian / Ubuntu
sudo apt install libxkbcommon-dev libwayland-dev libfontconfig1-dev
# Arch
sudo pacman -S --needed libxkbcommon wayland fontconfig
```

Running it also needs a **Vulkan driver**; the loader alone is not one. That is
`vulkan-radeon`, `vulkan-intel` or `nvidia-utils` on real hardware, and
`vulkan-swrast` (lavapipe) on a VM or a box with no GPU driver at all. A machine
with a working GL driver and no Vulkan can fall back with `ZESTERM_BACKEND=gl`;
if neither is there, zesterm reports which backends it tried and which were
compiled in.

Then, on any of the three:

```
cargo build --workspace
cargo xtask check-deps
cargo xtask check-spawn
```

## Packaging

`packaging/linux/` carries an Arch `PKGBUILD` (a `-git` package: it builds the
repository, since there is nothing tagged to download), a desktop entry and an
icon. The
entry's `StartupWMClass` and `Icon` must both equal `platform::APP_ID`: on
Wayland the taskbar icon is found by matching the window's `app_id` against an
installed desktop file, so a mismatch means no icon and nothing to point at.
`the_app_id_and_the_desktop_entry_agree` fails the build rather than letting the
two drift.

Nothing is packaged for Windows or macOS yet, and there is no release workflow.

## Theming

Theme `ui.*` tokens are [`@sigx/terminal-zero`](https://sigx.dev/terminal/docs/theming)'s
contract verbatim, so one theme file styles zesterm's chrome *and* any `@sigx/terminal-ui`
TUI running inside it. Importers exist for iTerm2 `.itermcolors`, Windows Terminal
schemes, base16/base24, and Alacritty/Ghostty TOML.

## License

MIT OR Apache-2.0

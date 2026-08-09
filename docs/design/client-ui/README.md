# Handoff: zesterm client UI — tabbed shell, command blocks, fleet, themes, mobile

## Overview

A clickable UI mockup for the zesterm client: the tabbed window chrome (horizontal
**and** vertical tab layouts), the command-block terminal pane, split panes, the
command palette, the fleet (host directory) view, the theme gallery, and the
blocks-first mobile client.

It exists to give WS-A (chrome), WS-E (command blocks) and WS-G (web client) a
single visual target so the desktop window, the browser tab and the phone do not
each invent their own vocabulary.

## About the design files

`Zesterm.dc.html` in this bundle is a **design reference built in HTML** — a
prototype of intended look and behaviour, not production code to copy. It is a
single-file streaming component with inline styles; there is no build step and
nothing in it is meant to ship.

The task is to **recreate these designs in the target environment**, using that
environment's established patterns:

- **`zest-app` (Rust / wgpu)** — chrome is GPU-drawn. The renderer already has the
  SDF rect pipeline and `Chrome { rects, glyphs }` with absolute-pixel
  `GlyphInstance`s; tab strip, block headers, status bar and palette are rect +
  glyph batches, not a DOM. Every colour below must come from resolved
  `zest_theme::UiTokens`, never a literal — the mock's hexes *are* obsidian's
  token values and are written out here only so the mapping is checkable.
- **`clients/web` (TypeScript, pnpm workspace)** — the grid canvas plus DOM chrome.
  Consume `@zesterm/proto`'s `GridView` for cells and the block index for headers.
  Theme tokens arrive as the same 24-key `ui` record, so drive CSS custom
  properties straight off it (`--zt-bg`, `--zt-panel`, …) rather than hard-coding.
- **Mobile** — blocks-first list UI; no grid renderer needed for the two screens here.

To open the reference: open `Zesterm.dc.html` in a browser. Bottom-centre pill
switches screens; `⌘K` / `Ctrl+K` opens the palette; tabs and the second block's
fold chevron are live.

## Fidelity

**High-fidelity.** Colours, type sizes, spacing and radii are final and are the
values to hit. Every colour traces to `crates/zest-theme/src/builtin.rs`
(`obsidian`, plus the four other built-ins in the theme gallery). Copy is
production copy. Icons are placeholders — see **Assets**.

Two things are deliberately *not* specified: real glyph metrics (the terminal
grid is the renderer's business, and the mock fakes it with a webfont), and the
exact motion curves for window/tab transitions.

---

## Design tokens

### Chrome palette — `obsidian`, verbatim from `zest-theme`

| Token | Hex | Used for |
|---|---|---|
| `ui.bg` | `#0b0f1a` | terminal surface, active tab fill |
| `ui.panel` | `#121829` | title bar cards, palette body, sidebar rows (selected uses `accentSoft`) |
| `ui.chrome` | `#0b0f1a` | window background |
| `ui.line` | `#2a3350` | all 1px borders |
| `ui.fg` | `#d7dcea` | primary text |
| `ui.dim` | `#8b93a7` | secondary text, inactive tab titles, terminal output body |
| `ui.faint` | `#4a516a` | metadata, cwd lines, timestamps, inactive dots |
| `ui.accent` | `#6ea8ff` | active-tab top rule, cursor, focus ring, palette caret, links |
| `ui.accentSoft` | `#16203a` | selected sidebar row, palette selected row, block-action chips |
| `ui.selSoft` | `#161d33` | hover fill on rows and tabs |
| `ui.success` | `#5fd17f` | exit 0, LAN-direct path, live host dot |
| `ui.warn` | `#e0b341` | running block, tunnel path, degraded link |
| `ui.danger` | `#e0606a` | non-zero exit, failed test |
| `ui.info` | `#5fc4e0` | adapter/protocol notices, host accents |
| `ui.magenta` | `#b07cff` | prompt user segment, third host accent |

Two darker surfaces appear in the mock that are **not** tokens and must be derived,
not literal: the title bar / sidebar fill (`#0d121f`) and the block-header fill
(`#0f1526`). Derive both as an OKLCH lightness step between `ui.chrome` and
`ui.panel` — in a light theme (`paper`) they must land *lighter* than `bg`, not
darker, which a literal would get wrong.

The desk background behind the window (`#05070d`) is presentation only.

### Other built-ins (theme gallery screen)

| Theme | bg | fg | accent (blue) | danger (red) |
|---|---|---|---|---|
| obsidian | `#0b0f1a` | `#d7dcea` | `#6ea8ff` | `#e0606a` |
| nord | `#2e3440` | `#d8dee9` | `#81a1c1` | `#bf616a` |
| gum | `#1f1b2a` | `#e6e1f0` | `#7aa2ff` | `#ff5c8a` |
| classic | `#000000` | `#e5e5e5` | `#0000ee` | `#cd0000` |
| paper | `#faf9f5` | `#24292f` | `#1f6feb` | `#c0392b` |

The eight-swatch strip under each preview is that theme's **normal** ANSI row in
index order (black, red, green, yellow, blue, magenta, cyan, white) — read them
from `builtin.rs`, do not re-type them. Brights are never shown because they are
derived (`oklch::brighten_ansi`).

### Typography

| Role | Family | Size | Weight | Line-height | Tracking |
|---|---|---|---|---|---|
| UI body / tab titles | Geist (fallback `system-ui`) | 12.5px | 400 | 1.4 | 0 |
| UI small / metadata | Geist | 11–11.5px | 400 | 1.4 | 0 |
| Section label (uppercase) | Geist | 10.5px | 600 | 1.4 | `.09em`, uppercase |
| Screen heading | Geist | 19px | 600 | 1.3 | `-.01em` |
| Mobile screen title | Geist | 21px | 600 | 1.2 | `-.02em` |
| Terminal grid + all shell text | JetBrains Mono | 12.5px | 400 | 1.62 | 0 |
| Chrome mono (cwd, keys, latency) | JetBrains Mono | 10–11px | 400 | 1.4 | 0 |
| Status bar | JetBrains Mono | 10.5px | 400 | 1 | 0 |

In `zest-app` the UI face is whatever `zest-font` resolves for chrome; the mock's
Geist stands in for "a neutral grotesque". The mono face is the user's configured
terminal font — do not ship a second one for chrome mono; reuse the grid font so
everything shares one atlas (per `docs/CONTRACTS.md`).

### Spacing, radii, shadows

- Spacing scale in use: 2, 4, 6, 8, 10, 12, 14, 16, 18, 22, 26, 34, 38, 44 px.
- Radii: 4px (dots/tiny chips), 5–7px (small chips, key caps), 8–9px (rows, tabs, buttons), 11–14px (cards, palette, panes), 29/38px (phone screen / phone body).
- Borders: always 1px `ui.line`. Dashed 1px `ui.line` for empty/asleep states.
- Shadows: window `0 24px 70px rgba(0,0,0,.6)`; palette `0 30px 80px rgba(0,0,0,.65)`; phone `0 20px 50px rgba(0,0,0,.55)`. In `zest-app` these come from `ui.shadow` × `effects.chrome_shadow_alpha`.
- Fixed sizes: title bar 46px, slim title bar 44px, status bar 28px, tab 34px, sidebar 262px, pane header 28px, palette 620px wide.

---

## Screens / views

### 1. Desktop — horizontal tabs (default)

**Purpose:** the everyday window. One session per tab, tabs may live on different hosts.

**Layout:** column. Title bar 46px → terminal pane (flex) → status bar 28px.

**Title bar** — background `#0d121f`, bottom border 1px `ui.line`.
- Left: three 11px traffic-light circles, 7px gap, `danger` / `warn` / `success`, 14px right padding. On Windows/Linux this block is replaced by the platform's own buttons on the *right*; the tab strip keeps its left origin either way.
- Centre: tab strip, 3px gap, bottom-aligned so the active tab's fill meets the pane.
- **Tab chip:** 34px tall, min 196px / max 240px wide, radius `9px 9px 0 0`, padding `0 11px`, 9px gap, `margin-bottom:-1px` so it overlaps the border.
  - Active: fill `ui.bg`, 1px `ui.line` on top/left/right, no bottom border, plus a 2px `ui.accent` inset rule along the top edge.
  - Inactive: transparent fill and border; hover → `ui.selSoft`.
  - Contents left→right: 6px host dot (active = the host's accent, inactive = `ui.faint`); two stacked lines — title 12.5px (`ui.fg` active / `ui.dim` inactive) and a 9.5px mono line `host · cwd` in `ui.faint`, both ellipsised; a 16px close affordance, `ui.faint`, hover fill `ui.line` + `ui.fg`.
- `+` new tab: 28×30, radius 7px, `ui.dim`, hover `ui.selSoft`.
- Right: two 26px pill buttons, radius 7px, 1px `ui.line`, 11px `ui.dim`, hover border `ui.accent` + text `ui.fg` — "⌘⇧E Vertical" and "⌘K".

**Tab data in the mock:** `zsh · studio · ~/dev/zesterm` (dot `#5fd17f`), `zestd — logs · crate · /var/log` (`#5fc4e0`), `build ×2 · studio + forge · split` (`#b07cff`).

**Status bar** — `#0d121f`, top border 1px `#1b2338`, 10.5px mono, `ui.faint` with
`ui.dim`/`ui.success` highlights, padding `0 14px`. Left: `~/dev/zesterm · ⎇ main* · 3 blocks`.
Right: `obsidian · ● LAN direct 0.3 ms`. The connection segment is `success` on a
direct/LAN path, `warn` on tunnel, `danger` while reconnecting.

### 2. Desktop — vertical tabs

**Purpose:** the fleet-scale layout. Sessions grouped by host, so "which machine" is structural rather than a badge.

**Layout:** row — 262px sidebar (`#0d121f`, right border `ui.line`) + main column with a slim 44px title bar and the same status bar.

**Sidebar,** top to bottom:
1. 44px header holding the traffic lights only.
2. Search affordance: 30px tall, radius 7px, `ui.panel` fill, 1px `ui.line`, `⌘K` in 11px mono + "Search sessions, blocks, hosts" in 12px `ui.faint`. 10px outer padding.
3. Host groups, 14px apart. Group header: 6px status dot + host name (10.5px, 600, `.09em`, uppercase, `ui.dim`) + a mono sub-label (`macOS · LAN 0.3ms`) in 10px `ui.faint`.
4. Session rows: 7px/8px padding, radius 8px, 9px gap. 5px state dot (running `warn` pulsing 1.6s, idle `ui.faint`, live `success`), then title 12.5px over a 10.5px mono cwd in `ui.faint`, then age right-aligned in 10px mono. Selected row fill `ui.accentSoft`; hover `ui.selSoft`.
5. Footer strip, 42px, top border `#1b2338`: `● 4 hosts online · 1 asleep` — clicking opens the fleet view.

**Slim title bar:** session name 13px, mono cwd in `ui.dim`, a 20px host chip (radius 6px, `ui.accentSoft` fill, `ui.accent` text, leading status dot), spacer, and a "Horizontal tabs" pill.

### 3. Terminal pane — command blocks

**Purpose:** the actual terminal, rendered as semantic blocks (OSC 133 A/B/C/D + OSC 7 cwd) rather than an undifferentiated scroll.

**Layout:** vertical scroll, 16px top padding, each block `0 18px 12px`.

**Block header:** 5px/12px padding, radius 8px, 10px gap, **2px left rail** coloured by
`BlockState` — `success` exit 0, `danger` non-zero, `warn` running, `ui.faint` for a
block with no output. Fill `#0f1526` when finished, `ui.panel` when running.
Contents: fold chevron (`▾`/`▸`, `ui.faint`), the command in 12.5px mono (state colour
when finished, `ui.fg` while running), spacer, then right-aligned metadata in 11px —
cwd (`ui.faint`), duration (`ui.faint`), and outcome: `exit 0` in `success`, `exit N`
in `danger`, or a running indicator (8px ring, 1.5px `warn`, transparent top, 0.9s
linear spin) plus `running 4.2s`. Folded headers additionally show `N lines`.

**Hover / focus on a block header** reveals two action chips, radius 5px,
`ui.accentSoft` fill, `ui.accent` text, 10px: `copy output ⌘⇧O` and `re-run ⌘⇧R`.
Both target the most recent block **with output**, not the block the cursor is in.

**Block body:** `8px 12px 0 24px`, 12.5px mono, `ui.dim`, `white-space: pre`, with SGR
colours applied per run. Collapsed when folded.

**Prompt line:** `4px 30px 18px`, 8px gap — user segment in `magenta`, `/` in `ui.faint`,
cwd in `ui.accent`, `⎇ main*` in `success`, `❯` in `ui.faint`, then an 8×16 `ui.accent`
block cursor blinking on a 1.1s step-end cycle.

**Exact mock content** (worth keeping — it is real output and exercises every state):
block 1 `cargo build --workspace`, exit 0, 51.2s, three `Compiling` lines + a green
`Finished`; block 2 `cargo xtask check-deps`, exit 0, 0.41s, foldable, 3 lines; block 3
`cargo run -p zest-render-wgpu --example render_dump`, running 4.2s.

### 4. Terminal pane — remote log tail

Same chrome, plain scrollback rather than blocks: 18px/22px padding, 12.5px mono, 1.7
line-height. Timestamps `ui.faint`, unit name `info`, warn lines `warn`, recovery lines
`success`, and a `▍` cursor in `ui.accent`. This screen exists to show what a *detached,
reconnecting* session looks like — stall, resync, coalesce, detach-but-keep-alive.

### 5. Split panes

**Purpose:** two sessions, two hosts, one tab.

Row of two flex-1 panes, each `margin:8px`, radius 10px, `overflow:hidden`.
Focused pane border 1px `ui.accent`; unfocused 1px `ui.line`.
**Pane header** 28px, padding `0 10px`, 8px gap: 5px host dot, host name (focused `ui.fg`,
else `ui.dim`), mono sub-label with cwd and path/latency in `ui.faint`, spacer, and the word
`focused` in 11px `ui.accent` on the active pane. Header fill `ui.panel` focused, `#0f1526` not.
Body: 12px padding, 12px mono, 1.65.

Open question flagged in review: whether the unfocused pane should also dim its body
text one step. Not specified here — decide with a real two-pane session in front of you.

### 6. Command palette

Modal over the window. Scrim `rgba(5,7,13,.66)` + 3px backdrop blur; panel top-aligned
88px down, 620px wide (92% max), radius 14px, `ui.panel` fill, 1px `ui.line`.

- **Query row:** 14px/16px padding, bottom border `#1b2338`. `❯` in `ui.accent`, query in 14px mono `ui.fg`, blinking 8×16 `ui.accent` caret, right-aligned `N hosts searched` in 10.5px `ui.faint`.
- **Results:** 8px padding. Group labels 10px, `.09em`, uppercase, `ui.faint`, `6px 10px`. Rows 9px/10px, radius 9px, 10px gap; selected `ui.accentSoft`, hover `ui.selSoft`. A row is: state glyph, primary text (mono for commands, sans for sessions/actions), right-aligned provenance (`studio · 2m ago · exit 0`) in 10.5px, and on the selected row a `⏎ re-run` hint in 10px `ui.accent`.
- **Groups, in order:** Blocks, Sessions, Hosts, Actions. Blocks first is the point — the palette is primarily a history of what ran anywhere in the fleet.
- **Footer:** `9px 16px`, top border `#1b2338`, fill `#0f1526`, 10.5px `ui.faint` with key caps in `ui.dim` mono — `↑↓ navigate`, `⏎ run here`, `⇧⏎ run on host…`, right-aligned `esc dismiss`.

### 7. Fleet

**Purpose:** the directory — which machines exist, are they up, how are we reaching them.

Padding `34px 38px`. Heading 19px/600 + a 12px `ui.faint` tagline, then a 12px `ui.dim`
lede (max 640px, `text-wrap: pretty`). Cards in
`grid-template-columns: repeat(auto-fill, minmax(300px, 1fr))`, 16px gap.

**Host card:** `ui.panel`, radius 12px, 18px padding, 1px `ui.line` (the local machine
gets `ui.accent`). Header: 8px status dot, 15px/600 name, optional 11px note
(`this machine`) or a 10.5px pill (`via tunnel`, `warn` on 12%-alpha warn fill).
Body: label/value rows, `justify-content: space-between`, 7px gap, 11.5px — labels
`ui.faint`, values `ui.dim`, fingerprints in 10.5px mono, counts in `ui.fg`.
Rows shown: `os`, `path` (`loopback 0.08 ms` / `LAN direct 0.4 ms` in `success`,
`tunnel 41 ms` in `warn`), `key` (`SHA256:…` truncated head+tail), `sessions`.

**Asleep card:** `#0f1526` fill, dashed `ui.line` border, everything in `ui.faint`,
rows `os` / `last seen` / `sessions (1 detached, kept)`, and a 28px "Wake over LAN"
button — radius 7px, 1px `ui.line`, hover border `ui.accent`.

### 8. Themes

Same page frame. Cards in `minmax(268px, 1fr)`, 18px gap, radius 12px, `overflow:hidden`,
1px `ui.line` (active card `ui.accent`).

Each card is three bands:
1. **Live preview** — 14px padding, 11px mono, 1.6, rendered *in that theme's own bg/fg*, showing three lines of plausible output with the theme's green/blue/red actually applied.
2. **Swatch strip** — 10px tall, eight equal flex children, the theme's normal ANSI row in index order, no gaps.
3. **Footer** — `9px 12px`, `ui.panel` fill: name 12.5px/500, a 10.5px `ui.faint` qualifier (`dark`, `light · default`), spacer, and `active` in 11px `ui.accent` on the selected one.

A final dashed card is the import target: "Import a scheme" over a 11px `ui.faint`
two-line list — `.itermcolors · Windows Terminal · base16 / base24 · Alacritty TOML`.

### 9. Mobile — session list

288×592 phone, body radius 38px, 10px bezel padding, screen radius 29px.

Status area 40px with a 78×20 pill notch and a 10.5px mono clock. Title block
`6px 16px 12px`: "Sessions" 21px/600/`-.02em` + `4 hosts · 9 sessions` in 11.5px `ui.faint`.
Host groups 12px apart: a dot + uppercase host label header, then a `ui.panel` card
(radius 12px, 1px `ui.line`) whose rows are 13px/14px with a 6px state dot, title 13.5px,
mono sub-line 10.5px `ui.faint`, and a `›` chevron. Rows within a card are separated by a
1px `#1b2338` divider, not a gap.

Tab bar 62px, top border `#1b2338`, three items (Sessions / Hosts / Blocks), 16px glyph
over a 10px label, active `ui.accent`, inactive `ui.faint`. **Every row is ≥44px tall —
keep it that way.**

### 10. Mobile — session, blocks-first

Header `4px 14px 12px`, bottom border `#1b2338`: back `‹` in 17px `ui.accent`, title
14px/500 over a 10px mono `studio · LAN 0.3 ms · live`, and a 6px `success` dot.

Body: block cards 10px apart, radius 11px, 1px `#1b2338` (running block `ui.line`).
Finished blocks are header-only — 10px/12px, 2px left rail in the state colour, command
in 11.5px mono ellipsised, duration right in 10px `ui.faint`. Tapping expands output;
long-press re-runs. The running block shows its rail in `warn`, the spinner ring instead
of a duration, and its output inline at 11px, 1.65.

Key bar, 8px/10px, top border `#1b2338`: 34px caps (radius 9px, `ui.panel`, 1px `ui.line`,
11.5px mono `ui.dim`) for `esc` `tab` `ctrl` `↑`, then a flex-1 `❯ run…` field with
`ui.accentSoft` fill, 1px `ui.accent` border, `ui.accent` text. The cap row scrolls
horizontally; `⌃` `⌥` `→` `/` `-` `|` follow off-screen.

---

## Interactions & behaviour

| Trigger | Result |
|---|---|
| Click a tab / sidebar row | Activate that session; pane content, status-bar cwd and git segment all follow |
| `⌘K` / `Ctrl+K` | Toggle the command palette; `Esc` or scrim click dismisses |
| `⌘⇧E` | Toggle horizontal ⇄ vertical tabs |
| `⌘D` | Split right (palette action row does the same, targeting a chosen host) |
| Click a block's chevron | Fold/unfold that block's output; header keeps a `N lines` count while folded |
| `⌘⇧O` | Copy the output of the most recent block **with output** |
| `⌘⇧R` | Re-run that same block |
| `⌘⇧` + click a block | Same two actions against any block in scrollback |
| Click the sidebar footer | Open the fleet view |
| Mobile: tap a block | Expand output. Long-press: re-run |

**Animations** — only four, all cheap:
- Cursor blink: 1.1s `step-end` infinite, opacity 1 → 0 at 50%.
- Running spinner: 0.9s linear infinite rotation of a 3/4 ring.
- Running-session dot: 1.6s ease-in-out pulse, opacity 1 → .35 → 1.
- Hover fills: instant in the mock. Use ≤120ms ease-out if the target platform makes it free; never animate tab *position*.

**Loading / degraded states worth implementing beyond the mock:** link stalled
(status-bar connection segment → `warn`, plus "buffering" text), reconnecting
(→ `danger`, chrome dims to `ui.dim`), host asleep (fleet card dashed variant),
and a block whose host went away mid-run (rail → `ui.faint`, metadata `interrupted`).

**Responsive:** below ~900px the sidebar collapses to a 48px icon rail (host dots only);
below ~640px the desktop layout is not used at all — that is the mobile client.
The tab strip never wraps to a second row; it scrolls, and the active tab is kept in view.

---

## State

Client-side state the UI needs, independent of the protocol:

- `layout: 'horizontal' | 'vertical'` — persisted per window.
- `activeSessionId: (HostId, SessionId)` — always the full pair, never a bare session id.
- `panes: { id, sessionId, focused }[]` per tab, with a split direction.
- `paletteOpen: boolean`, `paletteQuery: string`, `paletteSelection: index`.
- `foldedBlocks: Set<BlockId>` — per session, not global; survives resize because blocks re-anchor through the `Reindex`.
- `themeId: string` — the settings cascade owns this; the UI only reads the resolved palette.
- `screen: 'sessions' | 'hosts' | 'blocks'` on mobile.

Everything else — session list, cwd, block index, exit codes, durations, host
liveness, latency — is server state arriving over the control plane. The UI must
never compute or cache a block's outcome locally; blocks are parsed on the machine
the shell runs on and arrive whole.

---

## Assets

None to import. All iconography in the mock is a **placeholder**: solid circles for
status/host dots, a bordered ring for the spinner, text glyphs (`▾ ▸ › ‹ ❯ ⎇ ↺ × +`)
for everything else. Substitute the project's real icon set at implementation time —
in `zest-app` these become SDF rects or atlas glyphs, so pick shapes that survive that.

Fonts: JetBrains Mono and Geist are loaded from Google Fonts **in the mock only**.
Ship the user's configured terminal font for mono and the platform UI face for sans.

---

## Files

- `Zesterm.dc.html` — the prototype. Open in any browser; no build step. Bottom pill switches screens.
- `github.md` (project root, not bundled) — records the repo association and which repo files each screen was derived from.

Repo files the design was read from and must stay consistent with:

| Concern | Source of truth |
|---|---|
| Colour tokens, built-in themes | `crates/zest-theme/src/builtin.rs`, `crates/zest-theme/src/tokens.rs` |
| Block model and states | `crates/zest-core/src/blocks.rs`, `crates/zest-proto/src/delta.rs` |
| Chrome atlas / rect pipeline constraints | `docs/CONTRACTS.md` |
| Fleet model, LAN vs tunnel, sessions outliving windows | `docs/ARCHITECTURE.md` ADR-004…007 |
| Chrome and tab-strip work item | `docs/ROADMAP.md` WS-A; blocks WS-E; web client WS-G |

## Suggested check-in

Land the design reference and this handoff under `docs/design/` so the mock is
reviewable next to the ADRs it follows:

```
docs/design/client-ui/README.md        # this file
docs/design/client-ui/Zesterm.dc.html  # the prototype
```

Then implement per work stream — WS-A takes screens 1–3 and 5, WS-E takes 3 and 6,
WS-G takes the web client rendering of 1–4, and M4 takes 9–10. Reference this file
from the relevant ROADMAP checkboxes rather than restating measurements there.

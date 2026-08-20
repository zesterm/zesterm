# Handoff: zesterm client UI — tabbed shell, command blocks, fleet, themes, mobile

## Overview

A clickable UI mockup for the zesterm client: the tabbed window chrome (horizontal
**and** vertical tab layouts), the command-block terminal pane, split panes, the
command palette, the fleet (host directory) view, the theme gallery, the settings
editor (as a tab, generated from the config schema), the profiles editor (launch
targets pinned to hosts, each with its own appearance), and the blocks-first mobile
client.

It exists to give the chrome, command-blocks and web-client work a
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
- Fixed sizes: title bar 46px (both layouts), tab 34px, sidebar 262px, pane header 28px, palette 620px wide.

---

## Screens / views

### 1. Desktop — horizontal tabs (default)

**Purpose:** the everyday window. One session per tab, tabs may live on different hosts.

**Layout:** column. Title bar 46px → terminal pane (flex). Nothing below it.

**Title bar** — background `#0d121f`, bottom border 1px `ui.line`.
- Left: three 11px traffic-light circles, 7px gap, `danger` / `warn` / `success`, 14px right padding. On Windows/Linux this block is replaced by the platform's own buttons on the *right*; the tab strip keeps its left origin either way.
- Centre: tab strip, 3px gap, bottom-aligned so the active tab's fill meets the pane.
- **Tab chip:** 34px tall, flex-basis 168px, min 104px / max 232px wide, radius `9px 9px 0 0`, padding `0 11px`, 9px gap, `margin-bottom:-1px` so it overlaps the border.
  - Active: fill `ui.bg`, 1px `ui.line` on top/left/right, no bottom border, plus a 2px `ui.accent` inset rule along the top edge.
  - Inactive: transparent fill and border; hover → `ui.selSoft`.
  - Contents left→right: an 18px rounded glyph tile carrying the **profile's icon** in its **tab colour** on a 12%-alpha wash of it (inactive: `ui.faint`, no wash) — with a 6px `ui.info` **attention badge** on its top-right corner when the session has asked to be noticed and you have not looked yet, and **ringed** while the session is busy (a spinner for a running command or an indeterminate `OSC 9;4`, an arc for a percentage; `danger` ink when the job says it failed, `warn` when it warns) — the **title only** at 12.5px (`ui.fg` active / `ui.dim` inactive, `flex:1` + ellipsis), and a 16px close affordance (`ui.faint`, hover fill `ui.line`). **No second line** — host and cwd on a 34px chip made every tab look cramped and were unreadable at 9.5px; they live in the tab's `title` tooltip and the vertical sidebar and header, which have room for them. Chips are `flex:0 1 168px; min-width:104px; max-width:232px`.
- **New tab is a single `+`** — 32×30, borderless, 22px glyph, `ui.dim` ink, `ui.selSoft` fill
  with `ui.accent` ink while its menu is open. It **opens the launcher menu**; there is no
  separate caret and no default-only half. A split button was tried and dropped: two adjacent
  glyphs of different optical weight never looked like one control, and the disclosure is the
  more useful default action when profiles span hosts. The default profile is still one keystroke
  away — it is the menu's first row and `⏎` runs it. In the horizontal strip the
  box needs `margin-bottom:6px` — the strip is `align-items:flex-end` for the tab chips, so
  without it the button drops to the strip floor and reads as misaligned. These are not two launchers: one is the default
  action, the other is the menu. The default profile is also the menu's first row, tagged
  `default` on an `ui.accentSoft` fill, and the header says `⏎ runs the default`.
- **Profiles and Settings live in that menu**, not as permanent chips in the strip: they are
  tabs you open occasionally, and a chip each cost the session tabs ~180px of width. Picking
  either still opens it as a tab; the menu row shows `ui.accent` on `ui.accentSoft` while that
  tab is active.
- **The layout toggle is gone from the chrome.** Horizontal ⇄ vertical is `tabs.position` in
  Settings → Tabs, which applies live; a button duplicating a setting is a second source of
  truth.
- **Launcher menu:** 318px, radius 12, `ui.panel`, 1px `ui.line`, `0 20px 50px rgba(0,0,0,.62)`,
  right-anchored under the button. Rows: profile icon in its colour, name over its command
  line in 10px mono, the `default` tag where it applies, a host chip, and `⌘1…⌘9`. Then a
  divider and two actions — *Run on another host…* (`⇧⏎`), which opens the fleet directory to
  choose the machine, and *Manage profiles* (`⌘⇧,`). Both act; a dead row in a five-row menu
  reads as a broken feature.
- **Tab order:** session tabs, then Profiles, then Settings, then the split button — all
  packed left, with the spare space to their right. Profiles and Settings are ordinary tabs
  that happen to open last; nothing in the strip is right-aligned.
- **The scrolling row holds every tab.** The strip is an outer flex row containing an
  `overflow-x:auto`, `flex:0 1 auto` row of all tabs (Profiles and Settings included), then
  the `+` as a `flex:none` sibling, then a `flex:1` spacer. Three rules, each of which was a
  real defect first: the launcher must sit *outside* the scrolling row or its menu is clipped;
  chips are `flex:0 1 168px; max-width:232px` so they shrink rather than overflow; and they
  stop shrinking at `min-width:104px` — below that the label degrades to a single letter — at
  which point the row scrolls instead. The scrollbar is suppressed (`scrollbar-width:none`
  plus a `::-webkit-scrollbar` height of 0); a bar drawn across the tabs costs more than it
  explains. Keep the active tab scrolled into view.
- Right: two 26px pill buttons, radius 7px, 1px `ui.line`, 11px `ui.dim`, hover border `ui.accent` + text `ui.fg` — "⌘⇧E Vertical" and "⌘K".

**Tab data in the mock:** `zsh · studio · ~/dev/zesterm` (dot `#5fd17f`), `zestd — logs · crate · /var/log` (`#5fc4e0`), `build ×2 · studio + forge · split` (`#b07cff`).

**No status bar.** There deliberately isn't one: cwd and git branch are in the prompt line, the
block count is the blocks themselves, the theme is a Settings row, and host and link health are
carried by the tab's colour and icon, the vertical header's host chip, the fleet view and the
palette. A 28px strip restating all of it is a second source of truth for facts already on
screen. The one thing it did own alone was **link degradation**, which instead surfaces where it
matters: the affected tab and its pane (see *Loading / degraded states*).

### 2. Desktop — vertical tabs

**Purpose:** the fleet-scale layout. Sessions grouped by host, so "which machine" is structural rather than a badge.

**Layout:** the window is a **column** — one 46px header spanning the full width, then a row of
262px sidebar (`#0d121f`, right border `ui.line`) + pane. The header runs
edge to edge on purpose: on Windows the caption buttons sit top-right, and a header that stops at
the sidebar's edge leaves a dead gap above the sidebar with the search pill floating below it.
Full width also gives the window controls, the launcher and `⌘K` somewhere to live.

**Full-width header:** caption area (macOS lights left; on Windows the system buttons take the
right end and this row must reserve that space), the **active session's** name, cwd and host
chip, a spacer, and `⌘K`. The launcher is not here — it sits beside the sidebar search.

**Sidebar,** top to bottom:
1. Search row, flush under the header: the search pill (`flex:1`, 30px, radius 7px, `ui.panel`, 1px `ui.line`, `⌘K` in 11px mono + "Search sessions, blocks, hosts" in 12px `ui.faint`) with the **same `+` to its right** — searching and starting a session are the two things you do at the top of a sidebar. Its menu anchors `top:40px; left:0` from the button so it opens rightwards over the pane; right-anchored it runs off the window's left edge.
3. Host groups, 14px apart, **built from the same tab list the horizontal strip renders** — grouped by the host each tab runs on, so a session launched from a profile appears under that profile's host (`⬢ Ubuntu` under FORGE) and carries the same selection styling. Do not build this list from a separate array or from literal markup: a hardcoded row cannot be selected, and a truncated list cannot show a session the user just started. Group header: 6px status dot + host name (10.5px, 600, `.09em`, uppercase, `ui.dim`) + a mono sub-label (`macOS · LAN 0.3ms`) in 10px `ui.faint`.
4. Session rows: 7px/8px padding, radius 8px, 9px gap. 5px state dot, in precedence order: **attention `ui.info`** (the one state that is asking for *you*), **failed `danger`** (the program's own word about itself, which is newer than the block index's), **busy `warn` pulsing 1.6s** (a running block *or* an `OSC 9;4` report — neither implies the other), live `success`, idle `ui.faint`, then title 12.5px over a 10.5px mono cwd in `ui.faint`, then age right-aligned in 10px mono. Selected row fill `ui.accentSoft`; hover `ui.selSoft`.
   **The age's slot is also the close's**: under the pointer the age is replaced by
   the same 16px `×` the horizontal chip carries (`ui.faint`, hover fill `ui.line`),
   and the slot is reserved at the wider of the two either way so the title never
   reflows when the pointer arrives. A row 262px wide with a two-line label cannot
   spend width on both, which is why this one is revealed rather than always drawn
   — but a sidebar with no way to point at "close" is worse, and was the state
   before #379: middle-click, unadvertised, or nothing. The pinned Settings row
   still carries none (§11: app tabs have no close).
5. Footer strip, 42px, top border `#1b2338`: `● 4 hosts online · 1 asleep` — clicking opens the fleet view.

The header's session identity reads from the **active tab** — literal text there contradicts the pane the moment the active tab is not the first one.

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

**The state rail runs the block's full height** — header *and* output, not the
header alone — so a block is one object you can see the edges of. It lives in the
window padding, 2px wide with a 4px gap before the first column, and it is drawn
**in the grid layer, beneath the glyphs**. That is not an implementation detail
to tidy away: chrome paints over the text, so a rail drawn a layer up shaves the
left edge off column 0 on every output row, and with no padding to live in there
is nowhere honest to put it, so it is not drawn at all (the header keeps its own).

**Selecting a block.** Clicking its rail or its header selects it: rail to
`ui.accent`, header fill to `ui.accentSoft`, and a 10%-accent wash over the
output rows. A press anywhere in the grid proper clears it — two selections lit
at once, a block and a drag, would be two answers to "what does ⌘⇧O copy". Esc
does **not** clear it; that key belongs to the shell.

**Hover / focus on a block header** reveals a single 16px `⋯`, radius 5px,
`ui.accentSoft` fill, `ui.accent` glyph, right-aligned before the metadata. Its
slot is reserved whether or not it is drawn, so the metadata does not slide
sideways when the pointer arrives.

**The block menu** opens from the `⋯` or from a right-click on the block, panel
236px wide, radius 10, `ui.panel` on a hairline `ui.line` with a 20px shadow,
30px rows, flipping above its anchor near the foot of the pane. Rows, in order,
with a hairline between each group: `Fold`/`Unfold` · `Copy output ⌘⇧O`,
`Copy command`, `Copy command + output` · `Re-run ⌘⇧R`, `Re-run in new tab` ·
`Select block text`. A row that cannot apply is drawn in `ui.faint` and takes no
clicks — each is enabled by the very helper that performs it, so the menu can
never offer something that then does nothing.

**Right-click keeps the conhost convention where it is used:** a selection exists
→ copy it; else the row's block has output → its menu; else paste. The live
prompt has no output line, so right-click-to-paste at the prompt is untouched.

`⌘⇧O` and `⌘⇧R` act on the **selected** block, falling back to the most recent
block with output when nothing is selected — which is a session nobody has
clicked in, and at a prompt the cursor's own block has printed nothing.

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

### 11. Settings — a tab, not a modal

**Purpose:** editing the config file with pickers. It is a **tab**, not the overlay the
Rust currently opens: settings are a place you sit in and scroll, they need a category
list, and a modal cannot be left open beside a running shell.

**Where it lives in the chrome**

- **Horizontal layout:** an ordinary tab in the strip, **after the session tabs and before
  the `+`** — it takes its turn in the strip rather than being pinned anywhere. Same 34px
  chip geometry and active treatment as a session tab (`ui.bg` fill, 1px `ui.line`, 2px
  `ui.accent` inset top rule), but it sizes to its content instead of taking a 168px
  minimum, and it carries no host dot, no close × and no second line: a ⚙ glyph plus
  "Settings". At most one exists at a time.
- **Vertical layout:** a pinned row directly above the fleet footer, separated by a 1px
  `#1b2338` divider — ⚙, "Settings", and `⌘,` right-aligned in 10px mono. Selected row
  fill `ui.accentSoft`, text `ui.accent`.
- `⌘,` opens it; if it is already open it activates that tab rather than opening a second.
  Closing it is closing a tab.

**Layout:** row — 214px category rail + content column.

**Category rail** (`#0f1526`, right border 1px `#1b2338`):
1. Filter affordance at the top, 12px/10px padding: same 30px pill as the sidebar search,
   `/` in mono + "Filter settings".
2. Category rows: 30px, radius 8px, 12.5px, 8px gap. Selected `ui.accentSoft` fill +
   `ui.accent` text; unselected `ui.dim`. Right-aligned 10px mono **count of modified
   fields in that group** (blank when none) — `ui.accent` on the selected row, else `ui.faint`.
3. Footer note, 10.5px `ui.faint`, top border: that every field is generated from the schema.

**Categories, in this order** — the eight schema `x_zest_group` values, then one the schema
cannot express: `Text · Appearance · Window · Tabs · Shell · Scrolling · Cursor · Motion ·
Unknown keys`. Order is the client's call, per `ui.rs`; alphabetical is an artifact.

**Content header:** `22px 30px 16px`, bottom border 1px `#1b2338`. Group name 17px/600/`-.01em`,
its dotted prefix beside it in 10.5px mono `ui.faint`, and a 12px `ui.dim` lede (max 520px).

**Field row** — one shape for every widget, and it must be **width-responsive**: with both
the 262px session sidebar and the 214px category rail present the content column is under
400px, so the row wraps (`flex-wrap: wrap`) and the control drops to its own line,
right-aligned, rather than crushing the label column. Label column `flex: 1 1 260px;
min-width: 0`; control column `flex: 0 1 262px; min-width: 0; margin-left: auto`.

```
[dot] Label                                              [ control ]
      Description, ≤420px, 11.5px ui.dim, text-wrap: pretty
      dotted.key   [provenance chip]  [needs a restart]
```

`display:flex; align-items:flex-start; gap:20px; padding:16px 2px`, 1px `#1b2338` divider
between rows (none after the last). Control column is a fixed 262px, right-aligned.

- **Modified dot** — 5px, `ui.accent`, `margin-top:6px`; transparent when the value equals the
  schema default. **It is the reset button**: clicking it deletes the key from the file.
- **Provenance chip** — 10px, from `cascade::Source`: `set by config file` (`ui.accent` on
  `ui.accentSoft`), `set by profile <name>` (`magenta` on 12%-alpha magenta), `set by workspace
  config`, `set on the command line`. Absent when the value is a default. This is the whole
  point of keeping provenance: a user can see what is fighting them.
- **Restart chip** — `needs a restart` in `warn` on 12%-alpha warn, from `x_zest_restart`.
  Advisory only — `invalidate::class_of` is authoritative and covers more keys.

**Widget vocabulary** — `zest_config::ui::Widget`, all ten mapped:

| Widget | Treatment |
|---|---|
| `toggle` | 38×22 track, radius 11, 3px padding; 16px knob. On: `ui.accent` track, `ui.bg` knob, knob right. Off: `ui.line` track, `ui.dim` knob, knob left. |
| `number` | 30px stepper: − / mono value / ＋ in one 1px `ui.line` box, radius 8, `ui.panel` fill; 30px buttons, hover `ui.selSoft`. Value carries its unit (`14 pt`, `530 ms`, `1.25 ×`). Steps and clamps to the schema `minimum`/`maximum`; integer fields never render a decimal. |
| `slider` | 150px track 4px/radius 2 `ui.line`, `ui.accent` fill, 14px `ui.fg` knob, mono value right in a 44px column. Click-to-seek **and** drag, quantised to the field's step. |
| `select` | ≤3 short variants: segmented control — 3px padding, radius 9, `ui.panel` fill, 1px `ui.line`; selected segment `ui.accentSoft` + `ui.accent`. More than 3, or variants with doc comments: a 180px dropdown pill opening a 288px menu (radius 11, `ui.panel`, `0 18px 44px rgba(0,0,0,.6)`) whose rows show ✓, label, the kebab wire value in mono, and the variant's **doc comment** underneath. `window.backdrop` is the case that earns the menu. The menu also serves the **rosters** the client brings (`theme-picker`, `font-list`): past ~8 options it grows a 30px search row at the top, and the list scrolls and clips inside a panel bounded by the pane rather than growing past it — 266 installed families is the case that earns both, and the reason a roster used to open a centred command-palette modal instead of a dropdown at all. A search matching nothing says so rather than showing an empty panel. |
| `theme-picker` | 32px pill: a 60×14 five-swatch strip of the theme's own colours, the theme id, ▾. Opens the **same anchored dropdown** the `select` row describes, with a search row and a `Browse all themes…` footer that opens the gallery (screen 8) — the gallery shows each theme's swatches, which a 288px menu cannot, so it stays one click away rather than being the only door. The roster comes from the client, not the schema, which is why this needed the menu to accept one. Unset (`light_theme`) renders a dashed border and `not set` in `ui.faint`. This picker is the **window's** theme; a profile's grid palette is a different control (screen 12). |
| `text` | 32px mono input, `ui.panel`, 1px `ui.line`. Placeholder in `ui.faint` shows the resolved default (`/bin/zsh -l`). Under an edit it wears the accent ring and becomes a real text field: a 1.5px `ui.fg` caret at the insertion point, the selection behind the run in `ui.accentSoft`, and the run offset so the *caret* stays in view — the tail is not what you are editing. Arrows, ⌥←/→ by word, Home/End, ⌘A, and ⌘C/⌘X/⌘V (Ctrl+Shift too). |
| `path` | Same, plus a `Browse…` button (32px, 1px `ui.line`, hover border `ui.accent`). |
| `font-list` | Stacked 30px rows: ⠿ drag handle, family in mono, × to remove. The dashed row and Enter both open the searchable dropdown over installed families. Rows past the first resolvable face are dimmed and tagged `fallback`. Dashed `＋ Add a family` row at the end. Order is the setting — dragging is the edit. |
| `tag-list` | 26px chips, radius 7, `ui.accentSoft` fill, `ui.accent` mono text, × per chip; dashed `＋ tag` chip. A leading `-` is kept verbatim (`-liga` disables). |
| `key-value` | Paired 30px cells per entry — key in `ui.accent` mono (left radii), value in `ui.fg` mono (right radii); an empty value renders as `unset` in `ui.faint`, because empty *unsets* the variable. Dashed add row. |

**Two categories the schema cannot generate**

- **Profiles** are **not** a settings category — they have their own top-level tab (screen 12).
  `UI_EXCLUDED` skipping `profiles` in the schema walk is what makes that clean: the generated
  settings form never tries to render them as a field.
- **Unknown keys** — `Resolved::unknown_keys`, which the cascade keeps rather than discards.
  A `warn` banner explaining that a key from a newer version is indistinguishable from a typo,
  then a row per key: the key in mono, where it came from, and a suggestion when the edit
  distance to a real key is small (`did you mean size_pt?`).

**Footer bar** (42px, top border, `#0f1526`): an `ui.accent` dot, then the modified count as a
sentence — "3 settings differ from the defaults — click a dot to reset" — spacer, the config
file's path in 10.5px mono `ui.faint`, and `Edit as TOML`.

**Restart banner:** when a `restart_hint` field is written, a `warn` banner appears under the
header (radius 9, 10%-alpha warn fill, 35%-alpha warn border) with a `Relaunch now` action.

**Behaviour that is not cosmetic**

- Every edit is a **write to the settings file**, then a reload through the cascade — never a
  local mutation of a UI copy. The file stays the single source of truth, and the value the
  row shows is always the *resolved* one. This is already how `apply_theme` and the
  `tabs.position` flip work in `zest-app`; the editor must not invent a second path.
- Therefore a row can change **without being touched**: an external edit to `config.toml`, or
  a profile activating, updates the form live via the config watcher.
- `tabs.position` demonstrates this in the mock — switching it re-lays the chrome around the
  open settings tab while you are still in it.
- A field whose value came from a stronger layer is still editable; the write goes to the user
  file and the chip tells you it is being overridden. Do not disable the control.
- Filter matches key, label and description, and hides empty categories.
- Keyboard: ↑↓ move the selected row, ←→ adjust the selected field, ⏎ begins a text edit,
  Esc closes an edit then the filter then the tab — the discipline `settings_ui.rs` already has.

**Repo files this screen must stay consistent with:** `crates/zest-config/src/settings.rs`
(the tree, groups, widget hints, ranges, restart flags), `src/ui.rs` (`Widget`, `UiField`,
`UI_EXCLUDED`), `src/cascade.rs` (`Source`, provenance, `unknown_keys`),
`src/invalidate.rs` (what actually needs a restart), `crates/zest-app/src/settings_ui.rs`
(the existing overlay's row/edit model), and
`clients/web/packages/settings/src/fields.ts` + `generated/ui-fields.json` (the browser
reads the same walk — the web editor should render from that list, not a hand-written form).

### 12. Profiles — launch targets

> **A theme and a colour scheme are not the same thing, and this is the distinction the
> design turns on.** A `zest-theme` file carries two halves: the 24 chrome `UiTokens` and the
> ANSI palette. The **chrome half is the window's alone** — a per-tab chrome theme would
> repaint the titlebar, tab strip and status bar every time focus moved between tabs, which
> is why Windows Terminal keeps its app theme separate from its colour schemes too. The
> **palette half is the grid's**, and that is what a profile may override. So:
> `appearance.theme` (Settings → Appearance) styles the window and supplies the *default*
> palette; `color_scheme` (per profile) overrides the palette only. Per-profile identity in
> the chrome is carried by `tab_color` and `icon` — one accent and one glyph, not a repaint.
> Whether that accent comes from the **profile** or from the **host it runs on** is itself a
> per-profile, inheritable field (`color_from`): set it on Defaults and the whole fleet reads by
> machine. It belongs on the profile, not in global settings, because a profile that means
> "production" wants its own red wherever it runs.

**Purpose:** a profile is **what to run, which machine runs it, and how it looks**. That is
Windows Terminal's model with the one addition the fleet forces: the host is part of the
profile, so *"Ubuntu on forge"* is a single thing you can open, and a tab's colours tell you
which machine you are typing on. Profiles get their **own top-level tab**, beside Settings —
they are content, not preferences.

Backed by `Settings::profiles` (`BTreeMap<String, toml::Table>` — the settings tree again,
partially specified), so the editor is the same generated form scoped to a subtree, and the
cascade already resolves inheritance and provenance. `UI_EXCLUDED` skips `profiles` in the
settings walk precisely so this screen can own it.

**Layout:** row — 248px profile rail + editor column.

**Profile rail** (`#0f1526`, right border 1px `#1b2338`):
- Header: "Launch targets" (10.5px/600/`.09em`/uppercase, `ui.dim`) over an 11px `ui.faint` line.
- **Defaults** first, then one row per profile, with a 6px gap after Defaults so it reads as
  the parent rather than a sibling. Row: 24px glyph tile (profile icon, profile colour on a
  12%-alpha wash), name 12.5px, a 10px mono sub-line (`wsl.exe · forge`), and `⌘1…⌘9` right.
  Selected row `ui.accentSoft`.
- Footer: dashed `＋ New profile`, and a discovery line — *"Found on this fleet: 2 WSL
  distros, 1 SSH host. Generate profiles"*. Discovery is the feature that makes a fleet
  usable; Windows Terminal generates profiles for local distros, zesterm should generate them
  **per host**.

**Editor header:** 34px glyph tile in the profile's colour (12%-alpha fill, 33%-alpha border),
name 17px/600, a host chip, the command line in 11px mono `ui.faint`, and `Duplicate` /
`Delete` (the latter hovers to `ui.danger`). Defaults has no Delete.

**Live preview**, directly under the header: a miniature tab + grid. The **chrome fragment
stays in the window's theme** (`ui.panel` fill, `ui.line` border) and carries only a 2px top
rule in the profile's tab colour plus its icon; the **body** uses the profile's colour scheme
`bg`/`fg`/`accent`, its font size, its opacity, and its cursor shape and blink. A caption
under it says so in as many words: *"Chrome is the window's theme (obsidian). Only the grid
follows this profile's scheme."* Content is a real `uname -sr` and the
host's real answer (`Linux 6.8.0-31-generic`, `Microsoft Windows 10.0.22631`,
`Darwin 24.5.0`), with a PowerShell profile showing a `PS C:\src\zesterm>` prompt. This is the
row that makes per-profile appearance legible without launching anything.

**Sections and fields** — same field-row shape as Settings (label column `flex:1 1 260px`,
control column `flex:0 1 300px; margin-left:auto`, wrapping when narrow), grouped under
10.5px uppercase section rules:

| Section | Fields |
|---|---|
| **Launch** | `command` (mono text area — wraps; a WSL invocation is long), `host` (host pill: status dot, name, path+latency coloured `success` for LAN/loopback and `warn` for tunnel, ▾) plus an *Ask which host at launch* toggle for host-agnostic profiles, `starting_directory` (mono + `Browse…`; may be a path this machine has never heard of, e.g. `\\wsl$\Ubuntu-24.04\home\…`), `tab_title` (segmented: From shell / Profile name / Custom) |
| **Appearance** | `color_scheme` as a **swatch picker** — one 60×14 chip per scheme showing its **eight normal ANSI colours in index order**, name under it, selected chip bordered `ui.accent` on `ui.accentSoft`; `typography.families` (font pill) + `size_pt` (stepper); `window.opacity` (slider) + `backdrop` (segmented None/Mica/Acrylic/Vibrancy); `background_image` (a real drop target — `<image-slot>`, 96px, radius 9 — plus Fill/Fit/Watermark and a Dim slider); `tab_color` — a **Profile colour / Host colour** segmented choice first, then six 22px swatches (from the theme's accents, selected one ringed) which dim to 35% and stop taking clicks when the host decides — and `icon` (six 26px glyph tiles) |
| **Cursor** | `shape` (segmented) and `blink` (toggle) |

Deliberately **not** per-profile: padding, window size, and keybindings. They are window- or
app-level, and putting them here invites profiles that fight each other over one window.

**Inheritance is the whole interaction.** Every appearance row shows one of two chips:
`inherited from Defaults` (`ui.faint` on `#0f1526`, 1px `#1b2338`) or `overrides Defaults`
(`ui.accent` on `ui.accentSoft`), and the 5px modified dot only appears on an override.
Editing a row creates the override; clearing it falls back through Defaults. Launch fields
(command, host, directory) never show a chip — they are what makes the profile a profile, so
inheriting them is meaningless.

**Footer bar** (42px): a dot in the profile's colour, the override count as a sentence
("6 settings override Defaults" / "Every profile falls through to this one" on Defaults), the
TOML table name right-aligned in mono (`[profiles.ubuntu]`), and `Edit as TOML`.

**Behaviour**

- Selecting a profile in the launcher menu opens **a new tab running it**; selecting one in
  the rail only edits it. Two different verbs, two different places — a launcher row that
  opened the editor would duplicate the *Manage profiles* row three lines below it.
- **A just-launched tab has no history**, so it gets its own pane: the profile's scheme
  `bg`/`fg`/`accent`, its font size, its cursor, one line of provenance
  (*"New session · PowerShell on forge · pwsh.exe -NoLogo"*) in the scheme's dim colour, and a
  prompt with a live caret. Reusing another session's scrollback — which is what a shared
  blocks pane does — shows a macOS build log in a PowerShell tab.
- `⌘1…⌘9` launch the Nth profile. `⌘⇧,` opens this tab.
- A profile whose host is asleep still launches: the fleet wakes it (fleet card's *Wake over
  LAN*) and the tab shows the reconnecting state rather than failing.
- A profile's scheme applies to its **grid only**. The single per-tab chrome concession is the
  2px accent rule and glyph in the profile's `tab_color` — enough to read a mixed-host window
  at a glance, cheap enough that switching tabs is not a repaint.
- Consequence for `zest-app`: the glyph atlas and `Chrome { rects, glyphs }` keep **one**
  resolved `UiTokens` per window, and the ANSI palette is per-session state. A design that
  needed per-tab `UiTokens` would mean re-resolving chrome colour on every tab switch.

**Repo files this screen must stay consistent with:** `crates/zest-config/src/settings.rs`
(`Settings::profiles`, and every key a profile may override), `src/cascade.rs`
(`profile_layer`, `Source::Profile`, group-vs-value merge semantics — note `shell.env`
replaces wholesale so a profile can *clear* an inherited variable), `src/ui.rs`
(`UI_EXCLUDED`), and `crates/zest-theme` for the scheme swatches.

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
| `⌘,` | Open (or activate) the Settings tab |
| `⌘⇧,` | Open (or activate) the Profiles tab |
| Click `+` | Open the profile launcher (there is no second, default-only new-tab control) |
| `⏎` in the launcher | Run the default profile |
| Pick a profile in the launcher | Appends a tab running that profile on its pinned host, and focuses it |
| `⇧⏎` in the launcher | Run the highlighted profile on a host you choose |
| `⌘W` | Close the tab. On this machine's shells that ends them; a remote one only detaches |
| `⌘B` | Detach the tab — stop watching, leave the session running in the daemon |
| Click a modified dot | Reset that field — delete the key from the config file |
| Drag a slider / click its track | Set the value, quantised to the schema step |
| Mobile: tap a block | Expand output. Long-press: re-run |

**Animations** — only four, all cheap:
- Cursor blink: 1.1s `step-end` infinite, opacity 1 → 0 at 50%.
- Running spinner: 0.9s linear infinite rotation of a 3/4 ring.
- Running-session dot: 1.6s ease-in-out pulse, opacity 1 → .35 → 1.
- Hover fills: instant in the mock. Use ≤120ms ease-out if the target platform makes it free; never animate tab *position*.

**Loading / degraded states worth implementing beyond the mock:** link stalled (the affected
tab's glyph tile → `warn`, plus a "buffering" line in its pane), reconnecting (tab glyph →
`danger`, that pane's text dims to `ui.dim`), host asleep (fleet card dashed variant),
and a block whose host went away mid-run (rail → `ui.faint`, metadata `interrupted`).

**Responsive:** below ~900px the sidebar collapses to a 48px icon rail (host dots only);
below ~640px the desktop layout is not used at all — that is the mobile client.
The tab strip never wraps to a second row; it scrolls, and the active tab is kept in view.

**A tab can say it wants you.** The rule is one sentence and names no program:
*a tab earns the attention dot when its session emits a signal meaning "look at me",
at a moment when you were not looking at it.* Three inputs, all of them something a
program deliberately sends — `BEL`, `OSC 9 ; <text>`, and `OSC 777 ; notify ; …` —
so an agent CLI whose notification channel is the terminal bell, a `make` that rings
on failure, and a `notify-send` wrapper all light it without any of them being known here.

- **"Looking at" needs both halves**: the tab is active *and* the window has focus. The
  same bell arriving while zesterm sits behind a browser is precisely the case the dot
  exists for. It clears on activating the tab, and on the window regaining focus.
- **The mark differs by position because the surfaces do.** The chip gets a badge on its
  glyph tile — that tile's ink already carries link degradation, and one mark cannot
  honestly say two things — while the sidebar row has one dot and no link ink to collide
  with, so it recolours in place. Neither costs a pixel of the title's budget.
- **There is no unread bit on the host.** A latched flag would have to be cleared by
  someone, and with two devices watching one shell there is no answer to who — so the host
  reports the moment and every viewer keeps its own idea of what it has seen. A client that
  was not attached when the bell rang is simply never told, which is the right answer for a
  signal meaning "look at this now".
- `tabs.attention_bell` and `tabs.attention_notify`, both on by default, switch the two
  sources independently: a bell fires on tab-completion in some shells, a notification
  almost never fires by accident.
**Closing a tab is a choice, and only sometimes a question.** Closing the *window* has
always detached every tab, this machine's shells included — a session that cannot outlive
its window is the fleet negated (ADR-007) — while `⌘W` ends them, because that is what the
hand expects of it. The two disagree on purpose. What was missing was any way to ask for
the other outcome, and any warning before the destructive one:

- **`⌘B` detaches.** Its own chord, not a modifier on `⌘W`: the outcomes are not degrees of
  one another. `B` for background, and deliberately not `⌘⇧W` — every desktop chord must
  also be reachable as `Ctrl+Shift+<key>` on Windows, where a shifted letter collapses onto
  its unshifted twin's chord, so `⌘⇧W` and `⌘W` would be one gesture on the primary platform.
- **A confirm appears only when closing would destroy something** — a running command, or a
  full-screen program on the alternate screen (which records no OSC 133 markers at all, so
  it is the *usual* reason a TUI looks idle). 470×168, `ui.panel`, radius 12, the approval
  modal's scrim: it swallows rather than dismisses, because one of the three answers is
  irreversible. Buttons right to left — **Detach** in the affirmative corner (accent),
  **Close and stop it** (danger), **Cancel** — so the corner every dialog trains the hand to
  reach holds the answer that destroys nothing. `Esc` cancels; `⏎` detaches, and does nothing
  at all when there is no daemon to detach to.
- **`tabs.close_action`** (`kill` | `detach` | `ask`, default `kill`) and
  **`tabs.confirm_close_when_busy`** (default on) are the two settings. They are independent:
  one answers "which of these did you mean", the other "are you sure".

**A busy tab looks busy, in both positions.** `running` — the shell's word, from
OSC 133 — had been computed for every tab all along and drawn in exactly one place, the
sidebar's dot, so the horizontal strip showed nothing at all while a command ran. It now
rings the chip's glyph tile, and the animation clock is no longer gated to the sidebar.

Beside it, **`OSC 9;4`** is the program's own word about itself: `st` ∈ {0 clear, 1 set,
2 error, 3 indeterminate, 4 warning} with a percentage. Windows Terminal, WezTerm, ConEmu
and Ghostty all render it, which is what makes it interoperable rather than invented here.

- **The two are different facts and neither implies the other.** A block is silent under a
  shell with no integration and under the alternate screen; a program that reports progress
  may never mint a block. A tab with either is busy; a tab with both draws the more specific
  one.
- **The ring is separate from the dot inside the tile**, because that dot is the tab's
  *identity* — its profile, its host, the link's health — and a mark forced to choose
  between "which machine is this" and "is it busy" answers the less urgent question half
  the time.
- **A ring is a ring plus its gaps, not an arc**: an SDF box cannot draw one, so it is a
  circle with bites taken out in the colour of whatever is behind it. A spinner has one
  bite, orbiting; a bar erases the part that has not happened yet, and at 100% erases
  nothing — a closed ring, standing still. Spinner and bar are separate cases rather
  than one fraction, because 100% is exactly where inferring the first from the second
  makes *finished* render as *still going*. That background is a parameter and
  cannot be defaulted — a chip's differs between its active fill, the strip, and a hover
  fill, which is exactly why the block header's copy could hardcode one and this one cannot.
- **Reaching 0 after being busy, and reporting an error, both light the attention dot.**
  That is "my build finished" for a program that reports progress and never rings.

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

- `AGENTS.md` — read this first if you are a coding agent: ground rules, invariant checklist, verification protocol.
- `zesterm-demo.html` — **the runnable demo**: one self-contained file, opens in any browser, works offline. Bottom pill switches screens.
- `Zesterm.dc.html` — the prototype source (needs `support.js` + `image-slot.js` beside it). Edit this, not the bundle.
- `screenshots/` — one PNG per screen (924px wide), for visual comparison. Numbers come from this file, not from the pixels.
- `image-slot.js` — the drop-target component used by the profile background-image field. Design-time only; do not ship it.

Repo files the design was read from and must stay consistent with:

| Concern | Source of truth |
|---|---|
| Chrome tokens vs ANSI palette (the two halves of a theme file) | `crates/zest-theme/src/builtin.rs`, `crates/zest-theme/src/tokens.rs` |
| Block model and states | `crates/zest-core/src/blocks.rs`, `crates/zest-proto/src/delta.rs` |
| Chrome atlas / rect pipeline constraints | `docs/CONTRACTS.md` |
| Fleet model, LAN vs tunnel, sessions outliving windows | `docs/ARCHITECTURE.md` ADR-004…007 |
| Chrome, blocks and web-client work items | `docs/ROADMAP.md` § Open work |
| Settings tree, widgets, provenance | `crates/zest-config/src/{settings,ui,cascade,invalidate}.rs`, `crates/zest-app/src/settings_ui.rs`, `clients/web/packages/settings/` |
| Profiles as launch targets | `crates/zest-config/src/settings.rs` (`Settings::profiles`), `src/cascade.rs` (`profile_layer`, `Source::Profile`) |

Reference this file from the relevant ROADMAP items rather than restating
measurements there.

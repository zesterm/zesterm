# AGENTS.md — implementing the zesterm client UI from this design reference

You are a coding agent asked to build, review, or diff the zesterm client UI. This folder is
the **design source of truth** for the shell: window chrome, tab strip, command blocks,
profiles, settings, fleet, themes, mobile. Read this file first, then `README.md`.

## Read order

1. **`AGENTS.md`** (this file) — rules, invariants, verification protocol.
2. **`README.md`** — the full spec: tokens, then one section per screen with exact
   measurements. Sections are numbered 1–12 and referenced below.
3. **`screenshots/`** — rendered PNGs of every screen (1280px-wide window, 2x). Use these to compare visually.
4. **`zesterm-demo.html`** — the runnable demo, one self-contained file: double-click it, no build step, no network, works offline. Same content as the prototype below; this is the copy to hand to anyone who just wants to click through.
5. **`Zesterm.dc.html`** — the editable prototype source. Open it in a browser (no build step) and
   click the pill at the bottom to switch screens. When a measurement in `README.md` is
   ambiguous, the prototype's computed style is the tiebreaker; when the prototype and
   `README.md` disagree on *intent*, `README.md` wins.

## Ground rules

- **Do not port the prototype's code.** It is HTML/inline styles standing in for a wgpu
  chrome and a React web client. Port the *values* — hex codes, px sizes, radii, weights,
  spacing, copy, state logic — not the markup.
- **Tokens come from `zest-theme`, not from this folder.** Every colour in the mock is
  `obsidian` verbatim. If a hex here disagrees with `crates/zest-theme/src/builtin.rs`, the
  crate wins and the mock is stale — say so instead of hardcoding the hex.
- **Chrome text and ANSI text are two different palettes.** `ui.*` tokens style the shell;
  the 16 ANSI colours style terminal content. Never style chrome with an ANSI colour.
- **The settings and profiles editors render from a generated field list**, not a
  hand-written form: walk `zest-config`'s schema (`ui.rs`), render one row per field, and
  write every edit back to the config file so it reloads through the cascade. There is no
  second state path. See `README.md` §11.
- **Provenance is part of the UI.** Every settings row shows where its value came from
  (`cascade::Source`) and whether it needs a restart. Do not drop those chips.
- **Copy is designed.** Labels, empty states and hint text in the mock are final; changing
  them is a design change, not an implementation detail.
- **Iconography is placeholder.** Circles, chevrons and glyph stand-ins mark position and
  size only — substitute real icons at the same box size (`README.md` §Assets).

## Screenshot index

| File | Screen | Authoritative for |
|---|---|---|
| `01-tabs-horizontal.png` | §1 | Title bar, tab chips, `+`, `⌘K` pill, block pane, status bar |
| `02-tabs-launcher-menu.png` | §12 / §1 | The `+` launcher menu: profiles, other-host row, Profiles/Settings entries |
| `03-tabs-vertical.png` | §2 | Full-width header, host-grouped sidebar, search row with its own `+` |
| `04-split-panes.png` | §5 | Two panes, per-pane host identity, focused-pane treatment |
| `05-profiles.png` | §12 | Profile list rail, appearance/command/behaviour fields, scheme vs theme |
| `06-settings.png` | §11 | Every widget type, provenance chips, modified dots, category rail |
| `07-command-palette.png` | §6 | Scrim, panel geometry, result rows, footer key caps |
| `08-fleet.png` | §7 | Host cards, LAN vs tunnel, reachability |
| `09-themes.png` | §8 | Theme cards, preview swatches, import formats |
| `10-mobile.png` | §9–10 | Phone session list and blocks-first session view |

Screenshots are 1280px-wide window captures of the prototype at desktop scale; they are for visual
comparison, not for pixel measurement. Take numbers from `README.md`.

## Invariants to check before you call a screen done

These are the things reviews have caught repeatedly. Each is cheap to verify.

1. **Tabs pack left** in the horizontal strip, in order: session tabs, then `+`. Nothing is
   right-aligned except the `⌘K` pill.
2. **Chips are 34px** tall and hang 1px below the strip floor; the `+` box is centred on the
   chips' centre line, not on the strip.
3. **A tab chip shows its title only** — host and cwd live in the tooltip, status bar and
   sidebar. Chips stop shrinking at 104px and the strip scrolls past that.
4. **`+` opens the launcher menu**; there is no caret and no default-only half. The default
   profile is the menu's first row, run by `⏎`.
5. **In vertical layout the `+` sits right of the search pill**, and its menu opens
   rightwards (`left:0` from the button) so it stays inside the window.
6. **The vertical sidebar and the horizontal strip render from the same tab list.** A session
   launched from either appears under its profile's host, selected. No hardcoded rows.
7. **Launching actually launches**: a new tab gets its own fresh-session pane with the
   profile's scheme and prompt and one provenance line — it never inherits another session's
   scrollback.
8. **`tabs.position` lives in Settings → Tabs and applies live.** There is no layout toggle
   in the chrome.
9. **Exactly one item is lit** in any selection group (tab strip, sidebar, category rail,
   screen switcher).
10. **Full-screen views scroll** rather than clipping their last row, and no screen overflows
    horizontally at 640px content width or above.
11. **Terminal output wraps** (`pre-wrap`) inside panes — a clipped line loses the word that
    carries its meaning.
12. **Windows caption buttons** need reserved space at the right end of the title bar and of
    the vertical header; macOS lights sit at the left.

## Verification protocol

When you have an implementation to compare:

1. Open `zesterm-demo.html` and the equivalent implementation screen side by side at the same
   width.
2. Diff in this order: **structure** (what regions exist, in what order) → **geometry**
   (heights, paddings, radii from `README.md`) → **type** (family, size, weight, tracking) →
   **colour** (against `zest-theme`, not the mock) → **copy** → **states** (hover, focus,
   selected, modified, disconnected, running).
3. Run the invariant list above.
4. Report differences as either *implementation drift* (fix the code) or *stale design*
   (the mock disagrees with the crates — flag it, do not silently follow the mock).

## Work-stream mapping

WS-A: screens 1–3, 5. WS-E: 3, 6. WS-G: web-client rendering of 1–4. M4: 9–10. Screen 11
(Settings) replaces the current overlay. Screen 12 (Profiles) needs its own work item —
per-host profile discovery and per-tab theming cut across WS-A and the control plane.

Per-screen repo sources are in `README.md` §Files; the project-root `github.md` carries the
screen → repo file map used for syncing.

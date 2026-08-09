/**
 * Chrome surfaces the design uses that are deliberately *not* tokens —
 * `crates/zest-theme/src/derived.rs`, same math, same constants.
 *
 * The client-UI design paints three surfaces that do not exist in the sigx
 * vocabulary: the title-bar/sidebar fill, the fill of a finished block header,
 * and a hairline softer than `ui.line`. Adding tokens for them would break the
 * terminal-zero contract, and hard-coding the mock's hexes would paint a dark
 * bar onto a light theme — so they are derived, with the same OKLCH mixes as
 * the Rust crate, and the step runs in each theme's own direction.
 */

import type { UiTokens } from './tokens.ts';
import { mix } from './oklch.ts';

/**
 * The title bar and sidebar fill — an OKLCH step from `chrome` toward `panel`.
 * Sits nearer `chrome`: quieter than a panel, but separable from the terminal
 * surface it borders. (Obsidian: the mock's `#0d121f`.)
 */
export function titlebarFill(ui: UiTokens): string {
  return mix(ui.chrome, ui.panel, 0.32);
}

/**
 * The fill of a *finished* block header — nearer `panel` than the title bar,
 * because a header sits inside the terminal surface rather than framing it.
 * (A *running* block's header uses `ui.panel` itself.)
 */
export function blockHeaderFill(ui: UiTokens): string {
  return mix(ui.chrome, ui.panel, 0.65);
}

/**
 * A hairline softer than `ui.line`, for borders inside an already-bordered
 * surface: status-bar top edge, sidebar footer, palette footer. Full `line`
 * there would draw a louder edge than the panels it separates.
 */
export function softHairline(ui: UiTokens): string {
  return mix(ui.line, ui.chrome, 0.45);
}

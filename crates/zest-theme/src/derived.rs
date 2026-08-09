//! Chrome surfaces the design uses that are deliberately *not* tokens.
//!
//! The client-UI design (docs/design/client-ui/) paints three surfaces that do
//! not exist in the sigx vocabulary: the title-bar/sidebar fill, the fill of a
//! finished block header, and a hairline softer than `ui.line`. Adding tokens
//! for them would break the terminal-zero contract, and hard-coding the mock's
//! hexes would paint a dark bar onto a light theme — so they are derived here,
//! once, where every client (native, web, mobile) can call the same math
//! instead of each approximating it.

use crate::oklch;
use crate::tokens::{Rgba8, UiTokens};

/// The title bar and sidebar fill — an OKLCH step from `chrome` toward `panel`.
///
/// Sits nearer `chrome`: quieter than a panel, but separable from the terminal
/// surface it borders. In a light theme the step runs in that theme's own
/// direction, which is what a literal hex would get wrong.
#[must_use]
pub fn titlebar_fill(ui: &UiTokens) -> Rgba8 {
    oklch::mix(ui.chrome, ui.panel, 0.32)
}

/// The fill of a *finished* block header — nearer `panel` than the title bar,
/// because a header sits inside the terminal surface rather than framing it.
/// (A *running* block's header uses `ui.panel` itself.)
#[must_use]
pub fn block_header_fill(ui: &UiTokens) -> Rgba8 {
    oklch::mix(ui.chrome, ui.panel, 0.65)
}

/// A hairline softer than `ui.line`, for borders inside an already-bordered
/// surface: the status bar's top edge, the sidebar footer, the palette footer.
/// Full `line` there would draw a louder edge than the panels it separates.
#[must_use]
pub fn soft_hairline(ui: &UiTokens) -> Rgba8 {
    oklch::mix(ui.line, ui.chrome, 0.45)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin;
    use crate::oklch::luminance;

    /// The design handoff writes these surfaces out as hexes so the mapping is
    /// checkable (docs/design/client-ui/README.md, "Design tokens"). The
    /// derivation must land on them for obsidian — within quantization — or
    /// the mock and the app disagree about the everyday theme.
    #[test]
    fn obsidian_lands_on_the_mock_values() {
        let ui = builtin::obsidian().ui;
        let close = |got: Rgba8, want: (u8, u8, u8), what: &str| {
            let d = |a: u8, b: u8| (i16::from(a) - i16::from(b)).unsigned_abs();
            assert!(
                d(got.r, want.0) <= 4 && d(got.g, want.1) <= 4 && d(got.b, want.2) <= 4,
                "{what}: derived {got} strays from the mock's #{:02x}{:02x}{:02x}",
                want.0,
                want.1,
                want.2
            );
        };
        close(titlebar_fill(&ui), (0x0d, 0x12, 0x1f), "titlebar");
        close(block_header_fill(&ui), (0x0f, 0x15, 0x26), "block header");
        close(soft_hairline(&ui), (0x1b, 0x23, 0x38), "hairline");
    }

    /// The reason these are derived rather than literal: the step must run in
    /// each theme's own direction. Between `chrome` and `panel` in lightness,
    /// whichever way that theme's panel goes — the mock's dark hexes pasted
    /// into `paper` would fail this immediately.
    #[test]
    fn surfaces_sit_between_chrome_and_panel_in_every_theme() {
        for t in builtin::all() {
            let (lo, hi) = {
                let a = luminance(t.ui.chrome);
                let b = luminance(t.ui.panel);
                (a.min(b), a.max(b))
            };
            for (name, c) in [
                ("titlebar", titlebar_fill(&t.ui)),
                ("block header", block_header_fill(&t.ui)),
            ] {
                let l = luminance(c);
                assert!(
                    l >= lo - 0.002 && l <= hi + 0.002,
                    "{}: {name} fell outside the chrome..panel lightness band",
                    t.id
                );
            }
            assert!(
                crate::oklch::contrast_ratio(soft_hairline(&t.ui), t.ui.chrome)
                    < crate::oklch::contrast_ratio(t.ui.line, t.ui.chrome),
                "{}: the soft hairline is not quieter than line",
                t.id
            );
        }
    }
}

//! The chrome's palette, resolved from theme tokens.
//!
//! `UiTokens` is terminal-zero's contract and stays in sRGB strings; the
//! renderer wants premultiplied linear. The conversion happens exactly once,
//! here, when the theme or `window.chrome_opacity` changes — not per frame,
//! and never in the layout code, which should be arithmetic over finished
//! colours.
//!
//! ADR-003 discipline: `chrome_opacity` premultiplies into *fills* only.
//! Text is always full-alpha — translucent text is unreadable, and that rule
//! already holds for the grid.

use zest_render_wgpu::LinearRgba;
use zest_theme::{Rgba8, ThemeEffects, UiTokens};

/// Every colour the chrome layout needs, premultiplied linear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeColors {
    /// The strip / sidebar background.
    pub strip_bg: LinearRgba,
    /// Hairline separators: strip edge, sidebar edge.
    pub line: LinearRgba,
    /// The active tab's fill.
    pub tab_active_bg: LinearRgba,
    /// A hovered inactive tab's fill.
    pub tab_hover_bg: LinearRgba,
    /// The active tab's label.
    pub text_active: LinearRgba,
    /// Inactive tab labels.
    pub text_inactive: LinearRgba,
    /// De-emphasised detail: second lines, attached tags.
    pub text_faint: LinearRgba,
    /// The origin pill's fill on a healthy remote tab.
    pub pill_bg: LinearRgba,
    /// The origin pill's label.
    pub pill_text: LinearRgba,
    /// The origin pill's fill when the host is not reachable.
    pub pill_warn_bg: LinearRgba,
    /// Text on the warn pill.
    pub pill_warn_text: LinearRgba,
    /// Accent, for the selected picker row and "attached here" tags.
    pub accent: LinearRgba,
    /// Soft accent, for hover fills in the picker.
    pub accent_soft: LinearRgba,
    /// The picker's panel fill.
    pub panel_bg: LinearRgba,
    /// The scrim behind the picker.
    pub scrim: LinearRgba,
    /// Drop-shadow alpha for floating chrome (the picker panel).
    pub shadow_alpha: f32,
}

/// A fill: token alpha × chrome opacity, premultiplied.
fn fill(c: Rgba8, chrome_opacity: f32) -> LinearRgba {
    LinearRgba::from_srgb(c.r, c.g, c.b, f32::from(c.a) / 255.0 * chrome_opacity)
}

/// Text: token alpha only. Chrome opacity never applies to glyphs (ADR-003).
fn text(c: Rgba8) -> LinearRgba {
    LinearRgba::from_srgb(c.r, c.g, c.b, f32::from(c.a) / 255.0)
}

impl ChromeColors {
    #[must_use]
    pub fn new(ui: &UiTokens, effects: &ThemeEffects, chrome_opacity: f32) -> Self {
        let chrome_opacity = chrome_opacity.clamp(0.0, 1.0);
        Self {
            strip_bg: fill(ui.chrome, chrome_opacity),
            line: fill(ui.line, chrome_opacity),
            tab_active_bg: fill(ui.panel, chrome_opacity),
            // The hover fill is the active fill at reduced strength, derived
            // rather than a separate token so every theme gets a sane hover
            // without authoring one.
            tab_hover_bg: fill(ui.panel, chrome_opacity * 0.55),
            text_active: text(ui.fg),
            text_inactive: text(ui.dim),
            text_faint: text(ui.faint),
            pill_bg: fill(ui.accent_soft, chrome_opacity),
            pill_text: text(ui.accent_text),
            pill_warn_bg: fill(ui.warn, chrome_opacity * 0.35),
            pill_warn_text: text(ui.warn),
            accent: fill(ui.accent, chrome_opacity),
            accent_soft: fill(ui.accent_soft, chrome_opacity),
            // The picker floats above translucent chrome, so its panel is
            // always opaque: a see-through session list over a busy grid is
            // unreadable at exactly the moment the user is trying to read.
            panel_bg: fill(ui.panel, 1.0),
            scrim: fill(ui.shadow, 0.45),
            shadow_alpha: effects.chrome_shadow_alpha.unwrap_or(0.35).clamp(0.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 0.5)
    }

    #[test]
    fn chrome_opacity_dims_fills_but_never_text() {
        // The grid already enforces "opacity never applies to glyphs"; the
        // chrome must not reinvent translucent text.
        let c = colors();
        assert!(c.strip_bg.0[3] <= 0.5, "fills carry the chrome opacity");
        assert!(
            (c.text_active.0[3] - 1.0).abs() < 1e-6,
            "text stays full-alpha regardless of chrome opacity"
        );
    }

    #[test]
    fn the_picker_panel_is_opaque_whatever_the_chrome_is() {
        let c = colors();
        assert!(
            (c.panel_bg.0[3] - 1.0).abs() < 1e-6,
            "a translucent session list over a busy grid is unreadable"
        );
    }
}

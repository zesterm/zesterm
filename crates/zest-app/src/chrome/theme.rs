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
    /// The title bar / sidebar fill — `zest_theme::derived::titlebar_fill`,
    /// one lightness step off the window background (design handoff,
    /// "Design tokens").
    pub strip_bg: LinearRgba,
    /// Hairline separators: strip edge, sidebar edge — `ui.line`.
    pub line: LinearRgba,
    /// The softer hairline for borders inside an already-bordered surface:
    /// status bar top edge, sidebar footer, palette footer.
    pub hairline_soft: LinearRgba,
    /// The terminal surface — `ui.bg`. Also the active tab's fill, which is
    /// what makes the active tab read as part of the pane below it.
    pub bg: LinearRgba,
    /// The active tab's fill (= `bg`; kept as its own name so a future
    /// per-surface tweak has somewhere to land).
    pub tab_active_bg: LinearRgba,
    /// Hover fill on rows and tabs — `ui.selSoft`.
    pub tab_hover_bg: LinearRgba,
    /// A finished block header's fill — `zest_theme::derived::block_header_fill`.
    pub block_header_bg: LinearRgba,
    /// The active tab's label.
    pub text_active: LinearRgba,
    /// Inactive tab labels.
    pub text_inactive: LinearRgba,
    /// De-emphasised detail: second lines, cwd lines, timestamps.
    pub text_faint: LinearRgba,
    /// The origin pill's fill on a healthy remote tab.
    pub pill_bg: LinearRgba,
    /// The origin pill's label.
    pub pill_text: LinearRgba,
    /// The origin pill's fill when the host is not reachable.
    pub pill_warn_bg: LinearRgba,
    /// Text on the warn pill.
    pub pill_warn_text: LinearRgba,
    /// Accent, for the active tab's top rule, focus, links, selected-row hints.
    pub accent: LinearRgba,
    /// Soft accent, for the *selected* row and block-action chips.
    pub accent_soft: LinearRgba,
    /// Exit 0, LAN-direct path, live host dot.
    pub success: LinearRgba,
    /// Running block, tunnel path, degraded link.
    pub warn: LinearRgba,
    /// Non-zero exit, reconnecting.
    pub danger: LinearRgba,
    /// Adapter/protocol notices, second host accent.
    pub info: LinearRgba,
    /// Prompt user segment, third host accent.
    pub magenta: LinearRgba,
    /// The panel fill for floating chrome (picker, palette, settings).
    pub panel_bg: LinearRgba,
    /// The scrim behind modal chrome.
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
            strip_bg: fill(zest_theme::derived::titlebar_fill(ui), chrome_opacity),
            line: fill(ui.line, chrome_opacity),
            hairline_soft: fill(zest_theme::derived::soft_hairline(ui), chrome_opacity),
            bg: fill(ui.bg, chrome_opacity),
            tab_active_bg: fill(ui.bg, chrome_opacity),
            tab_hover_bg: fill(ui.sel_soft, chrome_opacity),
            block_header_bg: fill(zest_theme::derived::block_header_fill(ui), chrome_opacity),
            text_active: text(ui.fg),
            text_inactive: text(ui.dim),
            text_faint: text(ui.faint),
            pill_bg: fill(ui.accent_soft, chrome_opacity),
            pill_text: text(ui.accent_text),
            pill_warn_bg: fill(ui.warn, chrome_opacity * 0.35),
            pill_warn_text: text(ui.warn),
            accent: fill(ui.accent, chrome_opacity),
            accent_soft: fill(ui.accent_soft, chrome_opacity),
            // State colours are marks — dots, rails, exit codes — information
            // rather than surface, so like text they never dim with the chrome.
            success: text(ui.success),
            warn: text(ui.warn),
            danger: text(ui.danger),
            info: text(ui.info),
            magenta: text(ui.magenta),
            // The picker floats above translucent chrome, so its panel is
            // always opaque: a see-through session list over a busy grid is
            // unreadable at exactly the moment the user is trying to read.
            panel_bg: fill(ui.panel, 1.0),
            // 0.66, per the design's palette scrim. The mock backs it with a
            // 3px backdrop blur the rect pipeline cannot express; the heavier
            // scrim alone carries the separation.
            scrim: fill(ui.shadow, 0.66),
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

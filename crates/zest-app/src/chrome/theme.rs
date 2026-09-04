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
    /// A sunken surface inside the terminal's own —
    /// `zest_theme::derived::block_header_fill`. Named for the block header it
    /// used to fill; since #465 that is a row of text and this is the settings
    /// and profiles rails, their footers, an unfocused pane's header band and
    /// the picker's footer.
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
    /// The terminal surface at full alpha.
    ///
    /// Nothing paints this as a surface any more. Its remaining callers all
    /// want a *colour* rather than a fill: [`Self::pane_fill`] scales it by a
    /// profile's opacity, `App::block_bands` solves a wash against it, and the
    /// settings toggle's knob is drawn in it. An alpha would be wrong for all
    /// three. The full-pane screens this used to name take
    /// [`Self::screen_bg`] instead (#538).
    pub bg_opaque: LinearRgba,
    /// The ground under a full-pane screen — Settings, Profiles, Fleet, Themes.
    ///
    /// `window.chrome_opacity`, not the window's: a screen *is* chrome, and it
    /// is drawn as a surface exactly as the bars are, so this is an alpha onto
    /// whatever is behind the window rather than a tint toward the window's own
    /// background — #522's mechanism, reaching the screens it had left out.
    ///
    /// Why this may be glass where [`Self::panel_bg`] may not: when a screen
    /// owns the pane the terminal is not built at all (`pane_is_covered`), so
    /// there is no busy grid to be see-through over. A panel floats over one
    /// that is very much still running, and stays opaque for that reason.
    pub screen_bg: LinearRgba,
    /// The sunken rails and footers *inside* a full-pane screen, at the same
    /// alpha as the ground they sit on.
    ///
    /// A rail is not a card. `block_header_bg`'s other callers are objects on a
    /// surface — an unfocused pane's header band, the picker's footer, the
    /// inheritance chips — and those keep their opacity, like a bar's chips do.
    /// These two are the screen's *own* background continuing under a different
    /// tone: left opaque they read as a solid column between a glass sidebar
    /// and glass content, which is the discontinuity and not the structure.
    /// Pushed to `surface_rects` after the ground, which replaces rather than
    /// composites, so a rail simply takes over its own strip of it (#538).
    pub screen_rail_bg: LinearRgba,
    /// The panel fill for floating chrome (picker, palette, settings).
    pub panel_bg: LinearRgba,
    /// The scrim behind modal chrome.
    pub scrim: LinearRgba,
    /// Relative luminance the wash under a block's output should reach, and the
    /// luminance of the background it starts from.
    ///
    /// A block's wash is the state colour at some alpha, blended in **linear
    /// light** — and no single alpha serves every theme. The alpha that lifts
    /// `obsidian`'s near-black background into a visible panel moves `paper`'s
    /// near-white one by a single 8-bit step, which is the same trap
    /// `zest_theme::oklch::contrast_shift` documents for opaque surfaces, met
    /// one layer down by something that has to stay translucent.
    ///
    /// So the *step* is fixed and the alpha is solved: this is where
    /// `contrast_shift` would have landed, and `App::block_bands` asks what
    /// alpha of the state colour over [`Self::bg_opaque`] reaches it.
    pub wash_target: f32,
    pub wash_from: f32,
    /// Drop-shadow alpha for floating chrome (the picker panel).
    pub shadow_alpha: f32,
}

/// A fill: token alpha × chrome opacity, premultiplied.
fn fill(c: Rgba8, chrome_opacity: f32) -> LinearRgba {
    LinearRgba::from_srgb(c.r, c.g, c.b, f32::from(c.a) / 255.0 * chrome_opacity)
}

/// Text: token alpha only. Chrome opacity never applies to glyphs (ADR-003).
/// Relative luminance of an opaque linear colour (Rec. 709).
#[must_use]
pub fn luminance(c: LinearRgba) -> f32 {
    0.2126 * c.0[0] + 0.7152 * c.0[1] + 0.0722 * c.0[2]
}

fn text(c: Rgba8) -> LinearRgba {
    LinearRgba::from_srgb(c.r, c.g, c.b, f32::from(c.a) / 255.0)
}

impl ChromeColors {
    /// The terminal surface at `opacity`, for a tab whose profile overrides
    /// `window.opacity`.
    ///
    /// Arithmetic over a finished colour rather than a second resolve: a
    /// premultiplied fill scales linearly with its alpha, so the opaque
    /// surface this struct already holds is all it takes. That keeps this
    /// module's rule — tokens are converted once, never per frame — while
    /// letting one tab in the strip disagree with the window about how solid
    /// the pane under it is.
    #[must_use]
    pub fn pane_fill(&self, opacity: f32) -> LinearRgba {
        let a = opacity.clamp(0.0, 1.0);
        let [r, g, b, _] = self.bg_opaque.0;
        LinearRgba([r * a, g * a, b * a, a])
    }

    #[must_use]
    pub fn new(
        ui: &UiTokens,
        effects: &ThemeEffects,
        chrome_opacity: f32,
        window_opacity: f32,
    ) -> Self {
        let chrome_opacity = chrome_opacity.clamp(0.0, 1.0);
        let window_opacity = window_opacity.clamp(0.0, 1.0);
        // Which fills carry the chrome opacity is a design decision, not a
        // blanket rule: translucency belongs to the big background surfaces
        // — the title bar, the sidebar, the active tab's pane fill. The
        // design's structure lives *on* those surfaces (borders, the accent
        // rule, chips, selected rows), and multiplying 0.25 into a 2px rule
        // does not make the window glassy, it deletes the rule. Structure
        // stays full-strength, exactly as text always has (ADR-003).
        Self {
            strip_bg: fill(zest_theme::derived::titlebar_fill(ui), chrome_opacity),
            line: text(ui.line),
            hairline_soft: text(zest_theme::derived::soft_hairline(ui)),
            // `window.opacity`, not the chrome's: these two *are* the terminal
            // surface, and the active tab's whole job is to read as part of
            // the pane below it. Under `chrome_opacity` a glass strip over a
            // solid grid would cut a see-through notch into it, which is the
            // design's intent inverted.
            bg: fill(ui.bg, window_opacity),
            tab_active_bg: fill(ui.bg, window_opacity),
            tab_hover_bg: text(ui.sel_soft),
            // Not a block header's any more, despite the name — #465 made the
            // header a row of text on the grid, and the rule this alpha used to
            // keep ("a header *replaces* the prompt rows it covers, and a
            // translucent one double-prints the very text it exists to reword")
            // is kept by `zest_render_wgpu::BlockBand::header_to` instead, which
            // stops the grid drawing those rows at all.
            //
            // Still opaque, and still `1.0` on purpose: its remaining callers
            // are sunken *surfaces* — the settings and profiles rails and
            // footers, an unfocused pane's header band, the picker's footer —
            // and those are structure, not glass. Renaming it is a bigger sweep
            // than this change earns.
            block_header_bg: fill(zest_theme::derived::block_header_fill(ui), 1.0),
            text_active: text(ui.fg),
            text_inactive: text(ui.dim),
            text_faint: text(ui.faint),
            pill_bg: text(ui.accent_soft),
            pill_text: text(ui.accent_text),
            pill_warn_bg: fill(ui.warn, 0.35),
            pill_warn_text: text(ui.warn),
            accent: text(ui.accent),
            accent_soft: text(ui.accent_soft),
            // State colours are marks — dots, rails, exit codes — information
            // rather than surface, so like text they never dim with the chrome.
            success: text(ui.success),
            warn: text(ui.warn),
            danger: text(ui.danger),
            info: text(ui.info),
            magenta: text(ui.magenta),
            bg_opaque: fill(ui.bg, 1.0),
            screen_bg: fill(ui.bg, chrome_opacity),
            screen_rail_bg: fill(zest_theme::derived::block_header_fill(ui), chrome_opacity),
            // The picker floats above translucent chrome, so its panel is
            // always opaque: a see-through session list over a busy grid is
            // unreadable at exactly the moment the user is trying to read.
            panel_bg: fill(ui.panel, 1.0),
            // 0.66, per the design's palette scrim. The mock backs it with a
            // 3px backdrop blur the rect pipeline cannot express; the heavier
            // scrim alone carries the separation.
            scrim: fill(ui.shadow, 0.66),
            // 0.045 of perceptual lightness: a step you can see the edge of
            // without reading as a surface. Measured against the design's mock
            // on `obsidian`, then checked on the other four —
            // `a_block_wash_is_visible_in_every_builtin_theme` is what keeps a
            // new theme from shipping with invisible blocks.
            wash_target: luminance(fill(zest_theme::oklch::contrast_shift(ui.bg, 0.045), 1.0)),
            wash_from: luminance(fill(ui.bg, 1.0)),
            shadow_alpha: effects.chrome_shadow_alpha.unwrap_or(0.35).clamp(0.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 0.5, 1.0)
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
    fn a_full_pane_screen_is_glass_but_a_floating_panel_is_not() {
        // The line between the two, in one test so the pair cannot drift
        // apart. A screen owns the pane and the terminal is not built under it
        // (`pane_is_covered`), so it may be glass; the picker floats over a
        // grid that is still running, so it may not. Stated separately, the
        // second one is what gets "fixed" to match the first.
        let c = colors();
        assert!(
            (c.screen_bg.0[3] - 0.5).abs() < 1e-6,
            "a full-pane screen carries chrome_opacity, got {:?}",
            c.screen_bg
        );
        assert!(
            (c.screen_rail_bg.0[3] - 0.5).abs() < 1e-6,
            "and so does the rail inside it — it is the same surface, one tone down"
        );
        assert!(
            (c.panel_bg.0[3] - 1.0).abs() < 1e-6,
            "a panel over a live grid does not"
        );
        assert!(
            (c.block_header_bg.0[3] - 1.0).abs() < 1e-6,
            "nor do the objects sitting on a surface: pane header band, picker footer, inheritance chips"
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

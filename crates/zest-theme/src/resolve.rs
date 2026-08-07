//! Turning a [`Theme`] into concrete colours.
//!
//! Two directions, and they meet in the middle:
//!
//! - [`derive_ui_tokens`] goes *up*: a bare 16-colour scheme from iTerm2 or
//!   base16 has no notion of `panel`, `line`, `accentSoft` or `faint`, so those
//!   are derived. Getting this right is what decides whether an imported theme
//!   looks native or looks like a beautiful grid with a grey titlebar bolted on.
//! - [`resolve`] goes *down*: a complete theme becomes the flat 256-entry
//!   palette the terminal actually indexes into.

use crate::oklch;
use crate::tokens::{AnsiPalette, Rgba8, TerminalColors, Theme, ThemeMode, UiTokens};

/// A fully resolved palette. Mirrors `zest_core::PaletteSnapshot`.
///
/// Deliberately duplicated rather than shared: `zest-theme` must not depend on
/// `zest-core`, and `zest-core` must not depend on `zest-theme`. The app owns
/// the conversion, which is a dozen lines and keeps both crates independently
/// usable — the browser client wants this crate without a VT parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPalette {
    pub colors: [Rgba8; 256],
    pub foreground: Rgba8,
    pub background: Rgba8,
    pub cursor: Rgba8,
    pub cursor_text: Rgba8,
    pub selection_bg: Rgba8,
    pub selection_fg: Option<Rgba8>,
}

/// Resolve a theme into concrete colours.
pub fn resolve(theme: &Theme) -> ResolvedPalette {
    let ui = &theme.ui;
    let mut colors = [Rgba8::default(); 256];

    // 0..8 come from the theme's own ANSI names unless overridden. A theme
    // authored purely in the sigx vocabulary is therefore already complete.
    let normal = theme.ansi.normal.unwrap_or([
        ui.black, ui.red, ui.green, ui.yellow, ui.blue, ui.magenta, ui.cyan, ui.white,
    ]);

    // 8..16: an OKLCH lift, not a blend toward white. The two exceptions are
    // conventional rather than computed -- bright black is the theme's faintest
    // readable grey, and bright white its plain foreground.
    let bright = theme.ansi.bright.unwrap_or_else(|| {
        let mut b = [Rgba8::default(); 8];
        b[0] = ui.faint;
        for i in 1..7 {
            b[i] = oklch::brighten_ansi(normal[i]);
        }
        b[7] = ui.fg;
        b
    });

    colors[..8].copy_from_slice(&normal);
    colors[8..16].copy_from_slice(&bright);
    fill_extended(&mut colors);

    if let Some(overrides) = &theme.ansi.indexed {
        for (&i, &c) in overrides {
            colors[i as usize] = c;
        }
    }

    let t = &theme.terminal;
    let background = t.background.unwrap_or(ui.bg);
    let foreground = t.foreground.unwrap_or(ui.fg);
    let cursor = t.cursor.unwrap_or(ui.accent);

    ResolvedPalette {
        colors,
        foreground,
        background,
        cursor,
        // Text under a block cursor must be readable against the cursor itself.
        cursor_text: t
            .cursor_text
            .unwrap_or_else(|| oklch::best_contrast(cursor, background, foreground)),
        selection_bg: t.selection_bg.unwrap_or(ui.sel_soft),
        // `None` means selected text keeps its own colour, which preserves
        // syntax highlighting through a selection.
        selection_fg: t.selection_fg,
    }
}

/// Fill 16..=255 with the standard xterm cube and grayscale ramp.
///
/// Defined by convention rather than taste, so computed rather than stored --
/// 240 hex strings in every theme file is how a format becomes unmaintainable.
fn fill_extended(colors: &mut [Rgba8; 256]) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut i = 16usize;
    for &r in &LEVELS {
        for &g in &LEVELS {
            for &b in &LEVELS {
                colors[i] = Rgba8::rgb(r, g, b);
                i += 1;
            }
        }
    }
    for (n, idx) in (232usize..=255).enumerate() {
        let v = 8 + (n as u8) * 10;
        colors[idx] = Rgba8::rgb(v, v, v);
    }
}

/// Derive the full `ui.*` token set from a bare 16-colour scheme.
///
/// Every importer ends here, because no terminal colour scheme carries chrome
/// tokens. The derivations use perceptual mixing so an imported theme's tab bar,
/// borders and accents look like they belong to the same design as its grid.
#[must_use]
pub fn derive_ui_tokens(
    bg: Rgba8,
    fg: Rgba8,
    normal: [Rgba8; 8],
    bright: [Rgba8; 8],
    mode: ThemeMode,
) -> UiTokens {
    let _ = mode; // `contrast_shift` already reads lightness from `bg`.

    // Blue is the conventional accent, but a scheme whose blue is nearly grey
    // (some low-contrast themes) produces a lifeless UI, so fall back to cyan.
    let blue = normal[4];
    let accent = if chroma_of(blue) < 0.04 { normal[6] } else { blue };

    let panel = oklch::contrast_shift(bg, 0.03);
    let chrome = oklch::contrast_shift(bg, 0.015);
    // Borders have a requirement rather than a ratio: 14% toward the foreground
    // is visible on a mid-dark background but disappears on pure black, so mix
    // until it actually clears a contrast floor.
    let line = oklch::mix_to_min_contrast(bg, fg, 0.14, 1.15);
    let dim = oklch::mix(fg, bg, 0.35);
    let faint = oklch::mix(fg, bg, 0.62);

    UiTokens {
        bg,
        panel,
        chrome,
        line,
        // Shadows are black regardless of theme; their strength is alpha, which
        // is a rendering concern rather than a palette one.
        shadow: Rgba8::rgb(0, 0, 0),
        fg,
        dim,
        faint,
        accent,
        accent_soft: oklch::flatten(accent, 0.15, bg),
        accent_text: oklch::best_contrast(accent, bg, fg),
        sel_soft: oklch::flatten(accent, 0.22, bg),
        success: normal[2],
        warn: normal[3],
        danger: normal[1],
        info: normal[6],
        black: normal[0],
        red: normal[1],
        green: normal[2],
        yellow: normal[3],
        blue: normal[4],
        magenta: normal[5],
        cyan: normal[6],
        white: normal[7],
    }
    .with_bright_hint(bright)
}

impl UiTokens {
    /// Brights are not part of the sigx vocabulary, so they cannot live in
    /// `UiTokens`. They travel in [`AnsiPalette`] instead; this is a no-op that
    /// documents where they went, so a reader of `derive_ui_tokens` is not left
    /// wondering.
    fn with_bright_hint(self, _bright: [Rgba8; 8]) -> Self {
        self
    }
}

fn chroma_of(c: Rgba8) -> f32 {
    // Cheap stand-in for a full OKLCH conversion: how far the channels spread.
    let (r, g, b) = (f32::from(c.r), f32::from(c.g), f32::from(c.b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max <= 0.0 {
        0.0
    } else {
        (max - min) / 255.0
    }
}

/// Assemble a complete theme from a 16-colour scheme plus optional roles.
///
/// The shared tail of every importer.
#[must_use]
pub fn theme_from_scheme(
    id: &str,
    name: &str,
    bg: Rgba8,
    fg: Rgba8,
    normal: [Rgba8; 8],
    bright: [Rgba8; 8],
    terminal: TerminalColors,
) -> Theme {
    // Judge light vs dark from the background rather than trusting a name.
    let mode = if oklch::luminance(bg) > 0.4 { ThemeMode::Light } else { ThemeMode::Dark };
    let ui = derive_ui_tokens(bg, fg, normal, bright, mode);

    Theme {
        schema: crate::tokens::CURRENT_SCHEMA,
        id: id.to_string(),
        name: name.to_string(),
        mode,
        ui,
        ansi: AnsiPalette {
            normal: Some(normal),
            bright: Some(bright),
            indexed: None,
        },
        terminal: TerminalColors {
            foreground: terminal.foreground.or(Some(fg)),
            background: terminal.background.or(Some(bg)),
            ..terminal
        },
        effects: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheme() -> ([Rgba8; 8], [Rgba8; 8]) {
        let normal = [
            Rgba8::rgb(0x1e, 0x1e, 0x1e),
            Rgba8::rgb(0xcc, 0x33, 0x33),
            Rgba8::rgb(0x33, 0xaa, 0x55),
            Rgba8::rgb(0xcc, 0xaa, 0x33),
            Rgba8::rgb(0x33, 0x77, 0xcc),
            Rgba8::rgb(0xaa, 0x55, 0xcc),
            Rgba8::rgb(0x33, 0xaa, 0xcc),
            Rgba8::rgb(0xcc, 0xcc, 0xcc),
        ];
        let bright = normal.map(oklch::brighten_ansi);
        (normal, bright)
    }

    fn dark_theme() -> Theme {
        let (normal, bright) = scheme();
        theme_from_scheme(
            "test",
            "Test",
            Rgba8::rgb(0x0b, 0x0f, 0x1a),
            Rgba8::rgb(0xd7, 0xdc, 0xea),
            normal,
            bright,
            TerminalColors::default(),
        )
    }

    #[test]
    fn extended_palette_matches_xterm() {
        let p = resolve(&dark_theme());
        assert_eq!(p.colors[16], Rgba8::rgb(0, 0, 0));
        assert_eq!(p.colors[231], Rgba8::rgb(255, 255, 255));
        // 33 - 16 = 17 = 36*0 + 6*2 + 5, so r=0 g=2 b=5 -> #0087ff.
        assert_eq!(p.colors[33], Rgba8::rgb(0, 135, 255));
        assert_eq!(p.colors[232], Rgba8::rgb(8, 8, 8));
        assert_eq!(p.colors[255], Rgba8::rgb(238, 238, 238));
    }

    #[test]
    fn indexed_overrides_win_over_the_computed_cube() {
        let mut theme = dark_theme();
        let mut map = std::collections::BTreeMap::new();
        map.insert(33u8, Rgba8::rgb(1, 2, 3));
        theme.ansi.indexed = Some(map);

        let p = resolve(&theme);
        assert_eq!(p.colors[33], Rgba8::rgb(1, 2, 3));
        assert_eq!(p.colors[34], Rgba8::rgb(0, 175, 0), "neighbours are untouched");
    }

    #[test]
    fn brights_are_lighter_than_their_normals() {
        let p = resolve(&dark_theme());
        for i in 1..7 {
            assert!(
                oklch::luminance(p.colors[i + 8]) > oklch::luminance(p.colors[i]),
                "bright colour {i} is not brighter than its normal"
            );
        }
    }

    #[test]
    fn mode_is_detected_from_the_background_not_the_name() {
        let (normal, bright) = scheme();
        let light = theme_from_scheme(
            "l",
            "Light",
            Rgba8::rgb(0xfa, 0xfa, 0xf5),
            Rgba8::rgb(0x20, 0x20, 0x20),
            normal,
            bright,
            TerminalColors::default(),
        );
        assert_eq!(light.mode, ThemeMode::Light);
        assert_eq!(dark_theme().mode, ThemeMode::Dark);
    }

    #[test]
    fn derived_chrome_is_visible_against_the_background() {
        // The failure this guards: a panel identical to the background makes
        // the tab bar invisible, which is exactly how imported themes look
        // broken.
        let t = dark_theme();
        assert_ne!(t.ui.panel, t.ui.bg);
        assert_ne!(t.ui.line, t.ui.bg);
        assert!(oklch::contrast_ratio(t.ui.line, t.ui.bg) > 1.05);
    }

    #[test]
    fn derived_text_tokens_are_ordered_by_prominence() {
        let t = dark_theme();
        let c = |x| oklch::contrast_ratio(x, t.ui.bg);
        assert!(c(t.ui.fg) > c(t.ui.dim), "dim is quieter than fg");
        assert!(c(t.ui.dim) > c(t.ui.faint), "faint is quieter than dim");
    }

    #[test]
    fn foreground_is_readable_on_the_background() {
        let t = dark_theme();
        // WCAG AA for body text is 4.5:1. A theme failing this is unusable,
        // and importers can produce one from a bad source scheme.
        assert!(
            oklch::contrast_ratio(t.ui.fg, t.ui.bg) >= 4.5,
            "foreground/background contrast is below WCAG AA"
        );
    }

    #[test]
    fn accent_text_is_readable_on_the_accent() {
        let t = dark_theme();
        assert!(oklch::contrast_ratio(t.ui.accent_text, t.ui.accent) > 3.0);
    }

    #[test]
    fn a_greyish_blue_falls_back_to_cyan_for_the_accent() {
        // Some low-contrast schemes have a near-grey blue. Using it as the
        // accent produces a lifeless UI with no focal colour.
        let mut normal = scheme().0;
        normal[4] = Rgba8::rgb(0x70, 0x72, 0x74);
        let ui = derive_ui_tokens(
            Rgba8::rgb(0x0b, 0x0f, 0x1a),
            Rgba8::rgb(0xd7, 0xdc, 0xea),
            normal,
            normal,
            ThemeMode::Dark,
        );
        assert_eq!(ui.accent, normal[6], "should have fallen back to cyan");
    }

    #[test]
    fn cursor_text_contrasts_with_the_cursor() {
        // Under a block cursor the character is painted in cursor_text; if that
        // matches the cursor colour the character disappears while you type.
        let p = resolve(&dark_theme());
        assert!(oklch::contrast_ratio(p.cursor_text, p.cursor) > 3.0);
    }

    #[test]
    fn explicit_terminal_roles_override_the_derivation() {
        let mut theme = dark_theme();
        theme.terminal.cursor = Some(Rgba8::rgb(0xff, 0x00, 0xff));
        assert_eq!(resolve(&theme).cursor, Rgba8::rgb(0xff, 0x00, 0xff));
    }
}

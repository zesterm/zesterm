//! Cell geometry.
//!
//! # Two things that must be right
//!
//! **Cells are integers in *physical* pixels**, computed after applying the DPI
//! scale factor — not logical pixels rounded later. Fractional cell sizes cause
//! cumulative column drift across a wide row and make text shimmer while
//! scrolling. Logical values are derived from physical, never the reverse.
//!
//! **Extra line-height space is split above and below the baseline.** Dumping
//! it all below (the naive `baseline = ascent`) looks visibly broken at
//! `line_height = 1.5`, and it is one of those bugs that reads as "this
//! terminal feels off" without anyone being able to name it.

/// Typography inputs that change cell geometry.
///
/// Every field here forces a `ResizePseudoConsole`, because changing the cell
/// size changes how many columns fit. That is why these are grouped: they share
/// one invalidation path with font size and DPI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Typography {
    /// Font size in **points**, as every other terminal means it.
    ///
    /// Converted to pixels at 96 logical DPI, so 12pt is 16 physical pixels at
    /// 100% scaling — see [`Typography::size_px`].
    pub size_pt: f32,
    /// Multiplier on the font's natural line height. 1.0 is the font's own.
    pub line_height: f32,
    /// Extra horizontal space per cell, in logical pixels. May be negative.
    pub letter_spacing: f32,
    /// Cell advance as a multiple of the font size, or `0.0` for the font's own.
    ///
    /// A multiple of the *size* rather than of the font's natural advance,
    /// which is how Windows Terminal states it and therefore the number a user
    /// comparing the two already has. Most monospace faces sit near 0.6.
    ///
    /// `0.0` means "whatever this face says", which is the only sane default:
    /// the right absolute number depends on the font, so a fixed one would be
    /// wrong for every face but the one it was picked against.
    pub cell_width: f32,
    /// Physical pixels per logical pixel.
    pub scale_factor: f32,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            size_pt: 12.0,
            line_height: 1.2,
            letter_spacing: 0.0,
            cell_width: 0.0,
            scale_factor: 1.0,
        }
    }
}

impl Typography {
    /// Physical pixels per point, at 96 logical DPI.
    ///
    /// The DPI scaling factor is applied separately, so this is the constant
    /// that turns the *unit* into pixels and nothing more.
    pub const PX_PER_PT: f32 = 96.0 / 72.0;

    /// Font size in physical pixels — what the rasterizer actually needs.
    ///
    /// This used to be `size_pt * scale_factor`, which made the field's name a
    /// lie: the default 13 was a **13 pixel** face, about 9.75 real points,
    /// while Windows Terminal, WezTerm, kitty and Alacritty all specify points
    /// and default to 11–12 of them. zesterm therefore rendered noticeably
    /// smaller than every peer at the same number, and "the text is blurry"
    /// was in part just "the text is small".
    #[must_use]
    pub fn size_px(&self) -> f32 {
        (self.size_pt * Self::PX_PER_PT * self.scale_factor).max(1.0)
    }
}

/// Resolved cell geometry, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub cell_w: u32,
    pub cell_h: u32,
    /// Distance from the top of the cell to the baseline.
    pub baseline: u32,
    pub underline_y: u32,
    pub underline_thickness: u32,
    pub strikeout_y: u32,
}

impl CellMetrics {
    /// Derive cell geometry from a font's metrics.
    ///
    /// `advance` is the widest advance among representative characters rather
    /// than the font's `average_width`: a nominally monospace font can still
    /// report an average that clips `M` or `W`.
    #[must_use]
    pub fn derive(m: &swash::Metrics, advance: f32, typo: &Typography) -> Self {
        let natural_h = m.ascent + m.descent + m.leading;
        let cell_h = (natural_h * typo.line_height).round().max(1.0);

        let letter_spacing_px = typo.letter_spacing * typo.scale_factor;
        // An override replaces the face's advance rather than scaling it, so
        // the number means the same thing whichever font is loaded -- which is
        // the point of stating it against the size.
        let advance =
            if typo.cell_width > 0.0 { typo.size_px() * typo.cell_width } else { advance };
        let cell_w = (advance + letter_spacing_px).round().max(1.0);

        // Split the leftover evenly so the text sits centred in the cell.
        let extra = cell_h - natural_h;
        let baseline = (m.ascent + extra * 0.5).round().max(0.0);

        let thickness = m.stroke_size.round().max(1.0);

        // Clamp decorations into the cell, so a font with eccentric metrics
        // cannot paint outside its own row.
        //
        // The bounds have to be ordered *before* clamping. `f32::clamp` panics
        // when min > max, and that happens for real settings: a small
        // `line_height` can make `cell_h - 1` fall below `baseline + 1`. A user
        // typing `line_height = 0.5` should get ugly underlines, not a crash.
        let max_y = (cell_h - 1.0).max(0.0);
        let underline_min = (baseline + 1.0).min(max_y);
        let underline_y = (baseline - m.underline_offset).round().clamp(underline_min, max_y);
        let strikeout_y = (baseline - m.strikeout_offset).round().clamp(0.0, max_y);

        Self {
            cell_w: cell_w as u32,
            cell_h: cell_h as u32,
            baseline: baseline as u32,
            underline_y: underline_y as u32,
            underline_thickness: thickness as u32,
            strikeout_y: strikeout_y as u32,
        }
    }

    /// Columns and rows that fit in a window, after padding.
    #[must_use]
    pub fn grid_size(&self, width_px: u32, height_px: u32, padding: u32) -> (u16, u16) {
        let usable_w = width_px.saturating_sub(padding * 2);
        let usable_h = height_px.saturating_sub(padding * 2);
        let cols = (usable_w / self.cell_w).max(1).min(u16::MAX as u32) as u16;
        let rows = (usable_h / self.cell_h).max(1).min(u16::MAX as u32) as u16;
        (cols, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in metrics with round numbers, so the arithmetic is checkable by
    /// eye rather than by running it.
    fn metrics() -> swash::Metrics {
        swash::Metrics {
            units_per_em: 1000,
            glyph_count: 100,
            is_monospace: true,
            has_vertical_metrics: false,
            ascent: 16.0,
            descent: 4.0,
            leading: 0.0,
            vertical_ascent: 0.0,
            vertical_descent: 0.0,
            vertical_leading: 0.0,
            cap_height: 14.0,
            x_height: 10.0,
            average_width: 10.0,
            max_width: 10.0,
            underline_offset: -2.0,
            strikeout_offset: 6.0,
            stroke_size: 1.0,
        }
    }

    #[test]
    fn natural_line_height_uses_the_fonts_own_metrics() {
        let typo = Typography { line_height: 1.0, ..Default::default() };
        let cm = CellMetrics::derive(&metrics(), 10.0, &typo);
        assert_eq!(cm.cell_h, 20, "ascent + descent + leading");
        assert_eq!(cm.cell_w, 10);
        assert_eq!(cm.baseline, 16, "no extra space to distribute");
    }

    #[test]
    fn extra_line_height_is_split_above_and_below() {
        // 20px natural, 1.5x => 30px, so 10px extra. Bottom-aligning would put
        // the baseline at 16 and leave a 10px gap under the text, which looks
        // broken. Centring puts it at 21.
        let typo = Typography { line_height: 1.5, ..Default::default() };
        let cm = CellMetrics::derive(&metrics(), 10.0, &typo);
        assert_eq!(cm.cell_h, 30);
        assert_eq!(cm.baseline, 21, "half the extra space goes above the text");
    }

    #[test]
    fn cell_width_is_stated_against_the_font_size_not_the_face() {
        // The number a user brings from Windows Terminal has to mean the same
        // thing here, or "0.6" is a different width in each application.
        let m = metrics();
        let typo = Typography { size_pt: 12.0, cell_width: 0.6, ..Default::default() };
        let px = typo.size_px();
        // A face whose natural advance is nothing like 0.6 of the size: the
        // override must ignore it entirely rather than scale it.
        let wide = CellMetrics::derive(&m, px * 1.4, &typo);
        let narrow = CellMetrics::derive(&m, px * 0.2, &typo);
        assert_eq!(wide.cell_w, narrow.cell_w, "the face's own advance stops mattering");
        assert_eq!(
            wide.cell_w,
            (px * 0.6).round() as u32,
            "0.6 means 0.6 of the font size, as Windows Terminal states it"
        );
    }

    #[test]
    fn zero_cell_width_keeps_the_faces_own_advance() {
        // The default, and the only sane one: the right absolute number depends
        // on the font, so a fixed default would be wrong for every face but one.
        let m = metrics();
        let typo = Typography { cell_width: 0.0, ..Default::default() };
        assert_eq!(CellMetrics::derive(&m, 9.0, &typo).cell_w, 9);
        assert_eq!(CellMetrics::derive(&m, 13.0, &typo).cell_w, 13);
    }

    #[test]
    fn letter_spacing_widens_the_cell() {
        let typo = Typography { letter_spacing: 2.0, ..Default::default() };
        let cm = CellMetrics::derive(&metrics(), 10.0, &typo);
        assert_eq!(cm.cell_w, 12);

        let tight = Typography { letter_spacing: -2.0, ..Default::default() };
        assert_eq!(CellMetrics::derive(&metrics(), 10.0, &tight).cell_w, 8);
    }

    #[test]
    fn letter_spacing_is_scaled_by_dpi() {
        // A logical 2px of spacing must become 4 physical px on a 2x display,
        // or the grid gets tighter as DPI increases.
        let typo = Typography { letter_spacing: 2.0, scale_factor: 2.0, ..Default::default() };
        let cm = CellMetrics::derive(&metrics(), 20.0, &typo);
        assert_eq!(cm.cell_w, 24);
    }

    #[test]
    fn metrics_are_always_integral_and_nonzero() {
        // Fractional cell sizes cause column drift and shimmering text.
        let typo = Typography { size_pt: 0.1, line_height: 0.01, ..Default::default() };
        let cm = CellMetrics::derive(&metrics(), 0.2, &typo);
        assert!(cm.cell_w >= 1);
        assert!(cm.cell_h >= 1);
    }

    #[test]
    fn absurd_typography_does_not_panic() {
        // Found by the test above: a small line_height makes `cell_h - 1` fall
        // below `baseline + 1`, and f32::clamp panics when min > max. Config is
        // user input, so this has to degrade rather than crash.
        for line_height in [0.01, 0.1, 0.5, 0.9, 1.0, 4.0] {
            for letter_spacing in [-1000.0, -1.0, 0.0, 1000.0] {
                let typo = Typography { line_height, letter_spacing, ..Default::default() };
                let cm = CellMetrics::derive(&metrics(), 10.0, &typo);
                assert!(cm.cell_w >= 1 && cm.cell_h >= 1);
                assert!(cm.underline_y < cm.cell_h.max(1));
                assert!(cm.strikeout_y < cm.cell_h.max(1));
            }
        }
    }

    #[test]
    fn underline_stays_inside_the_cell() {
        // A font with an absurd underline offset must not paint outside its row.
        let mut m = metrics();
        m.underline_offset = -500.0;
        let cm = CellMetrics::derive(&m, 10.0, &Typography { line_height: 1.0, ..Default::default() });
        assert!(cm.underline_y < cm.cell_h);
        assert!(cm.underline_y > cm.baseline, "and below the baseline");
    }

    #[test]
    fn grid_size_accounts_for_padding() {
        let cm = CellMetrics { cell_w: 10, cell_h: 20, baseline: 16, underline_y: 18, underline_thickness: 1, strikeout_y: 10 };
        assert_eq!(cm.grid_size(1000, 400, 0), (100, 20));
        assert_eq!(cm.grid_size(1000, 400, 10), (98, 19), "padding on both sides");
    }

    #[test]
    fn a_tiny_window_still_has_one_cell() {
        let cm = CellMetrics { cell_w: 10, cell_h: 20, baseline: 16, underline_y: 18, underline_thickness: 1, strikeout_y: 10 };
        // Minimised or mid-drag windows hit this; returning 0 columns would
        // divide by zero downstream.
        assert_eq!(cm.grid_size(1, 1, 0), (1, 1));
    }
}

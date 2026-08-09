//! What the chrome takes from each window edge.
//!
//! The grid does not own the window; it gets the rectangle left over after
//! the padding and (soon) the tab strip have taken their share. Keeping that
//! subtraction in one type is what lets the strip move from the top edge to
//! the left without touching every place that converts between pixels and
//! cells — resize, redraw, pointer hit testing and IME placement all ask the
//! same `Insets` the same question.

use zest_font::CellMetrics;

/// Physical pixels taken from each window edge before the grid begins.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub top: f32,
    pub left: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Insets {
    /// Bare window padding on all four sides, no chrome.
    ///
    /// `padding` is in logical pixels — it is the user-facing
    /// `window.padding` setting — so it scales with the display. The old
    /// hard-coded constant was applied unscaled, which quietly halved the
    /// intended padding on every HiDPI display.
    #[must_use]
    pub fn padding_only(padding: u32, scale: f32) -> Self {
        let p = padding as f32 * scale.max(0.1);
        Self { top: p, left: p, right: p, bottom: p }
    }

    /// The grid's rectangle inside a window of `w` × `h` physical pixels,
    /// as `[x, y, width, height]` — the shape a `Viewport` wants.
    #[must_use]
    pub fn grid_rect(&self, w: u32, h: u32) -> [f32; 4] {
        let width = (w as f32 - self.left - self.right).max(0.0);
        let height = (h as f32 - self.top - self.bottom).max(0.0);
        [self.left, self.top, width, height]
    }

    /// Columns and rows that fit in the grid's rectangle.
    ///
    /// Mirrors `CellMetrics::grid_size`'s floor-and-clamp semantics — at
    /// least one cell each way, however small the window gets — but takes
    /// per-edge insets, which symmetric padding cannot express once a strip
    /// occupies one edge only.
    #[must_use]
    pub fn grid_dims(&self, m: CellMetrics, w: u32, h: u32) -> (u16, u16) {
        let [_, _, gw, gh] = self.grid_rect(w, h);
        let cols = ((gw as u32) / m.cell_w.max(1)).max(1).min(u32::from(u16::MAX)) as u16;
        let rows = ((gh as u32) / m.cell_h.max(1)).max(1).min(u32::from(u16::MAX)) as u16;
        (cols, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cell_w: u32, cell_h: u32) -> CellMetrics {
        CellMetrics {
            cell_w,
            cell_h,
            baseline: cell_h.saturating_sub(4),
            underline_y: cell_h.saturating_sub(2),
            underline_thickness: 1,
            strikeout_y: cell_h / 2,
        }
    }

    #[test]
    fn symmetric_insets_agree_with_the_font_crate() {
        // The old path was CellMetrics::grid_size(w, h, padding). Anything
        // this type computes for plain padding must match it, or the pty is
        // told a different size than the renderer draws.
        let m = metrics(10, 20);
        let i = Insets::padding_only(10, 1.0);
        assert_eq!(i.grid_dims(m, 1000, 400), m.grid_size(1000, 400, 10));
        assert_eq!(i.grid_dims(m, 1, 1), m.grid_size(1, 1, 10), "degenerate windows still agree");
    }

    #[test]
    fn padding_scales_with_the_display() {
        // 8 logical px on a 2x display is 16 physical. The unscaled constant
        // this replaces drew visibly tighter margins on every HiDPI screen.
        let i = Insets::padding_only(8, 2.0);
        assert_eq!(i.left, 16.0);
        assert_eq!(i.grid_rect(100, 100), [16.0, 16.0, 68.0, 68.0]);
    }

    #[test]
    fn asymmetric_insets_place_the_grid_off_centre() {
        // The whole reason this type exists: a top strip moves only the top
        // edge, a sidebar only the left one.
        let m = metrics(10, 20);
        let i = Insets { top: 38.0, left: 8.0, right: 8.0, bottom: 8.0 };
        assert_eq!(i.grid_rect(1000, 400), [8.0, 38.0, 984.0, 354.0]);
        assert_eq!(i.grid_dims(m, 1000, 400), (98, 17));
    }

    #[test]
    fn a_window_smaller_than_its_insets_still_yields_one_cell() {
        // A grid of zero cells panics deep in the terminal; clamping here is
        // what the font crate does and what the pty contract expects.
        let m = metrics(10, 20);
        let i = Insets { top: 200.0, left: 200.0, right: 200.0, bottom: 200.0 };
        assert_eq!(i.grid_dims(m, 100, 100), (1, 1));
        let [x, y, w, h] = i.grid_rect(100, 100);
        assert_eq!((w, h), (0.0, 0.0), "the rect clamps at zero rather than going negative");
        assert_eq!((x, y), (200.0, 200.0));
    }
}

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

/// Where a `cols` × `rows` grid sits inside `area`: centered when it is
/// smaller than the pane, exactly `area` otherwise.
///
/// Under size arbitration (#215) a session is the *smallest* attached
/// client's size, so this window's grid can be narrower or shorter than the
/// pane it is drawn in. Centering happens per axis and only on a strict
/// shortfall in whole cells; a grid that fills the pane's cell capacity gets
/// the pane's exact rectangle, sub-cell remainder and all, so a local session
/// renders byte-identically to what it did before arbitration existed.
///
/// A grid *larger* than the pane (the moment between a foreign grow and this
/// window's next resize) keeps the pane's rectangle and lets the renderer
/// clip, which is what it did before too.
///
/// Everything that converts between pixels and cells must use the same
/// rectangle — the renderer's viewport, pointer hit testing, IME placement
/// and the block headers — or a click lands one letterbox-offset away from
/// the glyph it visibly touched.
#[must_use]
pub fn letterbox(area: [f32; 4], cols: usize, rows: usize, m: CellMetrics) -> [f32; 4] {
    let cw = m.cell_w.max(1) as f32;
    let ch = m.cell_h.max(1) as f32;
    let fit_cols = ((area[2] / cw) as usize).max(1);
    let fit_rows = ((area[3] / ch) as usize).max(1);
    let [mut x, mut y, mut w, mut h] = area;
    if cols < fit_cols {
        w = cols as f32 * cw;
        // Floored to a whole pixel: a half-pixel origin samples every glyph
        // between texels and reads as a blurry font, not as an offset.
        x = area[0] + ((area[2] - w) / 2.0).floor();
    }
    if rows < fit_rows {
        h = rows as f32 * ch;
        y = area[1] + ((area[3] - h) / 2.0).floor();
    }
    [x, y, w, h]
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
    fn a_smaller_grid_is_centered_in_the_pane() {
        // The arbitration case (#215): another, smaller client owns the size,
        // and this window letterboxes -- per axis, in whole pixels.
        let m = metrics(10, 20);
        let area = [8.0, 38.0, 1000.0, 400.0]; // fits 100 x 20
        assert_eq!(
            letterbox(area, 60, 10, m),
            [8.0 + 200.0, 38.0 + 100.0, 600.0, 200.0],
            "a smaller grid must sit centered, not pinned top-left"
        );
        assert_eq!(
            letterbox(area, 60, 20, m),
            [208.0, 38.0, 600.0, 400.0],
            "the shortfall is per axis: full-height grids center only horizontally"
        );
    }

    #[test]
    fn a_grid_that_fills_the_pane_keeps_the_exact_pane_rect() {
        // The local-session case must be byte-identical to life before
        // arbitration: same origin, same width, sub-cell remainder and all --
        // this is what keeps every existing pixel assertion (#44) true.
        let m = metrics(10, 20);
        let area = [8.0, 38.0, 1005.0, 407.0]; // fits 100 x 20, with remainder
        assert_eq!(letterbox(area, 100, 20, m), area);
        // Transiently larger (a foreign grow this window has not resized to
        // yet): still the pane's rect, and the renderer clips as it always did.
        assert_eq!(letterbox(area, 140, 50, m), area);
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
